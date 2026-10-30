import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, type BrowserContext, type Page } from "patchright";
import { fingerprintAccountIdentity } from "../src/account";
import { SingleWindowCoordinator, type DriverExecution } from "../src/browser-window";
import { ChatGptBrowserDriver, type SingleBrowserSurface } from "../src/chatgpt-driver";
import { privateChromeExecutable } from "../src/chrome-executable";
import { activeComposer, composerText, insertExactPrompt, uniqueSendButton } from "../src/composer";
import { protocolBuild, renderActionPrompt, type ExecuteActionRequest } from "../src/protocol";

const native = process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME === "1";
const build = protocolBuild("pairing-submit-test");
const url = "https://chatgpt.com/c/pairing-submit-fixture";
const connector = "Fixture connector";
const oldPrompt = renderActionPrompt({kind:"pair_verify",reference:"A".repeat(43)});
const newReference = Buffer.alloc(32,1).toString("base64url");
const request: ExecuteActionRequest = {
  actionId:"pairing-submit",idempotencyKey:"pairing-submit-key",url,expectedUrl:url,
  pairingGeneration:2,connectorName:connector,model:null,effort:null,
  expectedAccountFingerprint:fingerprintAccountIdentity("fixture-user","fixture-account"),
  prompt:{kind:"pair_verify",reference:newReference},autoApproveToolCalls:false,
};
const prompt = renderActionPrompt(request.prompt);
let context: BrowserContext;
let browserContext: BrowserContext;
let page: Page;
let profile: string;
let initialDraft = "";
let ready = true;
let chipName = connector;
let quietGenerationMs = 0;
let generatingOnLoad = false;
let acceptsStop:boolean|number|"reload" = true;
let stopFinishDelayMs=200;
let fixtureLanguage:"en"|"el"|"it"|"ja"="en";
const fixtureUi={
  en:{send:"Send",stop:"Stop",counter:"of",efforts:["Instant","Medium","High","Extra High","Pro"]},
  ja:{send:"送信",stop:"停止",counter:"of",efforts:["Instant","中","高","非常に高い","Pro"]},
  el:{send:"Αποστολή",stop:"Διακοπή",counter:"από",efforts:["Γρήγορο","Μεσαίο","Υψηλό","Πολύ υψηλό","Pro"]},
  it:{send:"Invia",stop:"Interrompi",counter:"di",efforts:["Immediato","Medio","Alto","Molto alto","Pro"]},
} as const;
let localizedStopFallback=false;
let stopConfirmation: {delayMs?:number;persists?:boolean;preexisting?:boolean;captured?:boolean;duplicateSubmit?:boolean;unrelated?:boolean;changeUrl?:boolean;afterModelSelection?:boolean;trapFocus?:boolean;frame?:"url"|"srcdoc"|"nested"|"hidden"|"foreign"}|undefined;
const capturedStopDialogHtml=readFileSync(join(import.meta.dirname,'fixtures/stop-response-dialog.html'),'utf8');
// Same structural contract as ChatGPT, with localized labels and arbitrary Radix IDs.
const stopDialogHtml = `<div class="codex-dialog-overlay" style="position:fixed;inset:0;z-index:50;background:#0005"></div>
  <div role="dialog" id="radix-fixture" data-state="open" class="codex-dialog" aria-modal="true" aria-labelledby="radix-heading" aria-describedby="radix-description" style="position:fixed;inset:30% 20%;z-index:51;background:white">
    <div><form><h2 id="radix-heading" class="sr-only">Arrêter cette réponse ?</h2><p id="radix-description" class="sr-only">Vous pourrez envoyer un autre message.</p>
      <div class="heading-dialog">Arrêter cette réponse ?</div><div class="text-codex-description">Vous pourrez envoyer un autre message.</div>
      <div><button type="button">Continuer la réflexion</button><button type="submit" class="bg-primary-solid text-primary-solid">Arrêter</button></div>
    </form></div><button type="button">Fermer</button>
  </div>`;
let documentLoads = 0;
let stopReloadAt=2;
let loadedFixtureState="";
let sessionRequests = 0;
let sessionUnavailableBeforeLoad=0;
let changedAccountAfterReload = false;
let changedUrlAfterReload = false;
let shiftedEfforts = false;
let hideEffortLabels = false;
let nestedHistory = false;
let missingHistoryId = false;
let duplicateHistoryViews = false;
let destinationLocks:Record<string,number[]> = {};
let absentFamilies:string[] = [];
let duplicateEffortLabels = false;
let changeAccountAtRequest = Number.MAX_SAFE_INTEGER;
let initialModel = {family:"6",value:4,locked:[] as number[]};
const pill = (name: string) => `<span app-mention-path="app://fixture" app-mention-display-name="${name}" contenteditable="false">${name}</span> `;

beforeAll(async () => {
  if (!native) return;
  profile = await mkdtemp(join(tmpdir(),"pairing-submit-fixture-"));
  const executablePath = await privateChromeExecutable(join(import.meta.dirname,"../.browsers",`${process.platform}-${process.arch}`));
  context = await chromium.launchPersistentContext(profile,{channel:"chrome",executablePath,headless:true,viewport:null});
  browserContext=context;
  page = context.pages()[0]!;
  await installFixtureRoutes();
},30_000);

async function installFixtureRoutes(){
  await context.route("https://untrusted.example/**",route=>route.fulfill({contentType:"text/html",body:stopDialogHtml+`<script>document.querySelector('form').onsubmit=event=>{event.preventDefault();parent.postMessage('fixture-stop-confirmed','*');};</script>`}));
  await context.route("https://chatgpt.com/**",async route => {
    if (new URL(route.request().url()).pathname === "/api/auth/session") {
      sessionRequests++;
      if(documentLoads<sessionUnavailableBeforeLoad){await route.fulfill({json:{}});return;}
      await route.fulfill({json:{user:{id:"fixture-user"},account:{id:sessionRequests>=changeAccountAtRequest||changedAccountAfterReload&&documentLoads>=stopReloadAt ? "changed-account" : "fixture-account",planType:"pro",structure:"personal"}}});
      return;
    }
    if(new URL(route.request().url()).pathname==="/stop-confirmation-frame"){
      await route.fulfill({contentType:"text/html",body:"<!doctype html><html><body></body></html>"});return;
    }
    documentLoads++;
    const dialogHtml=stopConfirmation?.captured?capturedStopDialogHtml:fixtureLanguage==="it"?stopDialogHtml
      .replaceAll("Arrêter cette réponse ?","Interrompere questa risposta?")
      .replaceAll("Vous pourrez envoyer un autre message.","Puoi chiedere di nuovo o inviare un messaggio di follow-up quando vuoi.")
      .replaceAll("Continuer la réflexion","Continua a pensare").replaceAll("Arrêter","Interrompi").replaceAll("Fermer","Chiudi"):stopDialogHtml;
    await route.fulfill({contentType:"text/html",body:`<!doctype html><html><meta charset="utf-8"><style>[data-model-reasoning-effort-slider]{height:24px;min-width:240px}${stopConfirmation?.captured?'.codex-dialog-overlay{position:fixed;inset:0;z-index:50;background:#0005}.codex-dialog{position:fixed;left:50%;top:50%;transform:translate(-50%,-50%);z-index:51;background:white}':''}</style><body>
      <section id="history"></section>
      <form data-chatgpt-composer>
        <div role="textbox" contenteditable="true" style="white-space:pre-wrap;min-height:24px" data-composer-markdown><p>${pill(chipName)}${initialDraft}</p></div>
        <button type="submit" aria-label="${fixtureUi[fixtureLanguage].send}" disabled>${fixtureUi[fixtureLanguage].send}</button>
        <button type="button" id="effort" data-testid="model-switcher-dropdown-button" aria-haspopup="menu" aria-controls="picker" aria-expanded="false">Pro</button>
        ${generatingOnLoad ? '<button type="button" data-fixture-stop '+(localizedStopFallback?'':'data-testid="stop-button"')+' aria-label="'+(fixtureUi[fixtureLanguage].stop)+'">'+(fixtureUi[fixtureLanguage].stop)+'</button>' : ''}
      </form>
      <div id="picker" role="menu" hidden>
        <div data-model-picker-view="advanced">
          <button role="menuitemradio" data-family="6" aria-checked="true">GPT-6</button>
          <button role="menuitemradio" data-family="5.6" aria-checked="false">GPT-5.6 Sol</button>
          <button role="menuitemradio" data-family="5.5" aria-checked="false">GPT-5.5</button>
        </div>
        <div id="range" role="menuitem" tabindex="0" aria-describedby="effort-description"></div>
        <span id="effort-description"></span>
      </div>
      <div data-mention-list-scroll-area hidden><button data-list-navigation-item="true" data-highlighted=""><span>${connector}</span></button></div>
      <script>
        if (${changedUrlAfterReload&&documentLoads>=stopReloadAt}) history.replaceState({},'', 'https://chatgpt.com/c/changed-after-reload');
        const editor=document.querySelector('[contenteditable="true"]');
        const form=document.querySelector('form');const send=form.querySelector('button');
        const mentions=document.querySelector('[data-mention-list-scroll-area]');
        window.family=${JSON.stringify(initialModel.family)};
        window.definitions={
          '6':{min:0,value:4,labels:${JSON.stringify(fixtureUi[fixtureLanguage].efforts)},locked:[]},
          '5.6':{min:0,value:4,labels:${JSON.stringify(fixtureUi[fixtureLanguage].efforts)},locked:[]},
          '5.5':{min:7,value:11,labels:${JSON.stringify(shiftedEfforts ? ['Instant','Medium','High','Extra High','Pro'] : ['Turbo','Instant','Medium','High','Pro'])},locked:[]}
        };
        window.definitions[window.family].value=${initialModel.value};
        window.definitions[window.family].locked=${JSON.stringify(initialModel.locked)};
        for(const [family,locked] of Object.entries(${JSON.stringify(destinationLocks)}))window.definitions[family].locked=locked;
        for(const family of ${JSON.stringify(absentFamilies)})document.querySelector('[data-family="'+family+'"]')?.remove();
        if(${duplicateEffortLabels})window.definitions['6'].labels[3]='High';
        const control=document.querySelector('#effort'),picker=document.querySelector('#picker'),range=document.querySelector('#range');
        window.renderModel=()=>{
          const model=window.definitions[window.family],index=model.value-model.min;
          range.innerHTML='<div data-model-reasoning-effort-slider><span role="slider" aria-valuemin="'+model.min+'" aria-valuemax="'+(model.min+model.labels.length-1)+'" aria-valuenow="'+model.value+'"></span>'+model.labels.map((label,i)=>'<span data-selected="'+(i===index)+'" data-locked="'+model.locked.includes(i)+'" aria-label="'+(${hideEffortLabels}?'':label)+'"></span>').join('')+'</div>';
          document.querySelector('#effort-description').textContent=${fixtureLanguage==="ja" ? "model.labels[index]+'、'+model.labels.length+'件中'+(index+1)+'件目。'" : "window.family+' '+model.labels[index]+', '+(index+1)+"+JSON.stringify(" "+fixtureUi[fixtureLanguage].counter+" ")+"+model.labels.length+'.'"};
          control.textContent=model.labels[index];document.body.dataset.family=window.family;document.body.dataset.effort=model.labels[index];
          document.querySelectorAll('[data-family]').forEach(row=>row.setAttribute('aria-checked',String(row.dataset.family===window.family)));
        };
        control.onclick=()=>{picker.hidden=false;control.setAttribute('aria-expanded','true');};
        document.querySelectorAll('[data-family]').forEach(row=>row.onclick=()=>{document.body.dataset.familyChanges+=(row.dataset.family+",");window.family=row.dataset.family;window.renderModel();});
        range.onkeydown=event=>{
          if(!['ArrowLeft','ArrowRight'].includes(event.key))return;
          event.preventDefault();const model=window.definitions[window.family],step=event.key==='ArrowRight'?1:-1;let next=model.value+step;
          document.body.dataset.effortChanges=String(Number(document.body.dataset.effortChanges||0)+1);
          while(next>=model.min&&next<model.min+model.labels.length&&model.locked.includes(next-model.min))next+=step;
          if(next>=model.min&&next<model.min+model.labels.length)model.value=next;
          window.renderModel();
        };
        document.addEventListener('keydown',event=>{if(event.key==='Escape'){
          const closingPicker=control.getAttribute('aria-expanded')==='true';
          picker.hidden=true;control.setAttribute('aria-expanded','false');
          if(stopConfirmation?.afterModelSelection&&closingPicker&&!document.body.dataset.lateStopDialog){
            document.body.dataset.lateStopDialog='true';
            form.insertAdjacentHTML('beforeend','<button type="button" data-fixture-stop data-testid="stop-button" aria-label="Stop">Stop</button>');
            showStopDialog(form.querySelector('[data-fixture-stop]'));
          }
        }});
        window.renderModel();
        document.body.dataset.effortChanges="0";document.body.dataset.familyChanges="";
        editor.focus();const initialCaret=document.createRange();initialCaret.selectNodeContents(editor);initialCaret.collapse(false);
        window.getSelection().removeAllRanges();window.getSelection().addRange(initialCaret);
        document.body.dataset.sends="0";document.body.dataset.stops="0";document.body.dataset.order="";document.body.dataset.confirmations="0";window.enableSend=${ready};
        const stopConfirmation=${JSON.stringify(stopConfirmation??null)};
        const finishStop=stop=>setTimeout(()=>{stop.remove();document.body.dataset.order+='stopped,';document.body.dataset.stoppedAt=String(Date.now());},${stopFinishDelayMs});
        const showStopDialog=stop=>{
          if(document.querySelector('[role="dialog"], [data-stop-dialog-frame]'))return;
          const show=(dialogDocument,frame)=>{
            dialogDocument.body.insertAdjacentHTML('beforeend',${JSON.stringify(dialogHtml)});
            const dialog=dialogDocument.querySelector('[role="dialog"]'),dialogForm=dialog.querySelector('form');
            if(stopConfirmation.trapFocus){form.inert=true;dialogForm.querySelector('[type="submit"]').focus();}
            if(stopConfirmation.unrelated){dialogForm.querySelector('h2').textContent='Delete this conversation?';dialogForm.querySelector('.heading-dialog').textContent='Delete this conversation?';dialogForm.querySelector('[type="submit"]').textContent='Delete';}
            if(stopConfirmation.duplicateSubmit)dialogForm.querySelector('[type="submit"]').after(dialogForm.querySelector('[type="submit"]').cloneNode(true));
            if(stopConfirmation.changeUrl)history.replaceState({},'', 'https://chatgpt.com/c/changed-during-stop');
            dialogForm.addEventListener('submit',event=>{
              event.preventDefault();document.body.dataset.confirmations=String(Number(document.body.dataset.confirmations)+1);document.body.dataset.order+='confirm,';document.body.dataset.confirmedAt=String(Date.now());document.body.dataset.draftOnConfirmation=editor.textContent;
              if(!stopConfirmation.persists){dialog.remove();dialogDocument.querySelector('.codex-dialog-overlay').remove();frame?.remove();form.inert=false;editor.focus();finishStop(stop);}
            });
          };
          if(!stopConfirmation.frame){show(document);return;}
          const attach=(owner,nested=false)=>{
            const frame=owner.createElement('iframe');frame.dataset.stopDialogFrame='true';
            frame.style.cssText='position:fixed;inset:0;width:100%;height:100%;border:0;z-index:100';
            if(stopConfirmation.frame==='hidden')frame.style.display='none';
            frame.onload=()=>{
              if(stopConfirmation.frame==='foreign')return;
              if(stopConfirmation.frame==='nested'&&!nested)attach(frame.contentDocument,true);
              else show(frame.contentDocument,document.querySelector('[data-stop-dialog-frame]'));
            };
            if(stopConfirmation.frame==='foreign')frame.src='https://untrusted.example/stop-confirmation';
            else if(stopConfirmation.frame==='url')frame.src='/stop-confirmation-frame';
            else frame.srcdoc='<!doctype html><html><body></body></html>';
            owner.body.append(frame);
          };
          window.addEventListener('message',event=>{if(event.data==='fixture-stop-confirmed')document.body.dataset.confirmations=String(Number(document.body.dataset.confirmations)+1);});
          attach(document);
        };
        if(stopConfirmation?.preexisting)showStopDialog(form.querySelector('[data-testid="stop-button"]'));
        form.addEventListener('click',event=>{
          const stop=event.target.closest('[data-fixture-stop]');
          if (!stop) return;
          document.body.dataset.stops=String(Number(document.body.dataset.stops)+1);
          document.body.dataset.order+='stop,';
          if(stopConfirmation)setTimeout(()=>showStopDialog(stop),stopConfirmation.delayMs??0);
          else if (${typeof acceptsStop==="number" ? `Number(document.body.dataset.stops)>=${acceptsStop}` : acceptsStop==="reload" ? documentLoads>=stopReloadAt : acceptsStop}) finishStop(stop);
        });
        editor.addEventListener('input',()=>{
          mentions.hidden=!editor.textContent.startsWith('@');
          if (!mentions.hidden) return;
          send.disabled=true;
          if (window.enableSend) setTimeout(()=>{
            // History hydration happens after composer insertion, before Send becomes usable.
            document.querySelector('#history').innerHTML=${JSON.stringify('<section data-turn-id-container="old">'+(nestedHistory?'<div data-turn-id-container="old">':'')+'<article '+(missingHistoryId?'':'data-turn-id="old" ')+'data-testid="conversation-turn-old" data-turn="assistant"><div data-message-author-role="assistant"><div class="markdown"><p>Old answer</p></div></div><button data-testid="copy-turn-action-button">Copy</button></article>'+(nestedHistory?'</div>':'')+'</section>')};
            if(${duplicateHistoryViews})document.querySelector('#history').insertAdjacentHTML('beforeend',document.querySelector('#history').innerHTML);
            send.disabled=false;
          },300);
        });
        editor.addEventListener('keydown',event=>{
          if (event.key==='Enter' && !mentions.hidden) {
            event.preventDefault();editor.innerHTML=${JSON.stringify("<p>"+pill(chipName)+"</p>")};mentions.hidden=true;
            const range=document.createRange();range.selectNodeContents(editor);range.collapse(false);
            const selection=window.getSelection();selection.removeAllRanges();selection.addRange(range);
          }
        });
        form.addEventListener('submit',event=>{
          event.preventDefault();document.body.dataset.order+='send,';document.body.dataset.sends=String(Number(document.body.dataset.sends)+1);document.body.dataset.sentText=editor.textContent;document.body.dataset.sentAt=String(Date.now());
          document.body.dataset.sentFamily=window.family;document.body.dataset.sentEffort=document.body.dataset.effort;
          editor.innerHTML='';send.disabled=true;
          const turnKey='new-'+document.body.dataset.sends;
          document.querySelector('#history').insertAdjacentHTML('beforeend','<section data-turn-key="'+turnKey+'"><div data-user-message-bubble>Pair verification</div><div data-content-search-unit-key="'+turnKey+'-answer"><div data-conversation-role="assistant"><div class="markdown"></div></div></div></section>');
          if (!location.pathname.includes('/c/')) history.replaceState({},'', 'https://chatgpt.com/c/new-created-fixture');
          const finish=()=>{
            document.querySelector('[data-testid="stop-button"]')?.remove();
            document.querySelector('[data-turn-key="'+turnKey+'"] [data-conversation-role="assistant"]').innerHTML='<div class="markdown"><p>Verified.</p></div><button data-testid="copy-turn-action-button">Copy</button>';
          };
          if (${quietGenerationMs}>0) {
            form.insertAdjacentHTML('beforeend','<button type="button" data-testid="stop-button">Stop</button>');
            setTimeout(finish,${quietGenerationMs});
          } else finish();
        });
      </script></body></html>`});
  });
}

afterAll(async () => {
  await context?.close();
  if(browserContext!==context)await browserContext?.close();
  if (profile) await rm(profile,{recursive:true,force:true});
});

const surface: SingleBrowserSurface = {
  page:async()=>page,
  navigate:async destination=>{await page.goto(destination);loadedFixtureState=fixtureState();return page;},
  reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{},
};

// Synthetic server-side changes must be installed explicitly. Browser actions
// now reuse healthy pages, so these fixtures cannot rely on a production reload.
function fixtureState(){return JSON.stringify([initialDraft,ready,chipName,quietGenerationMs,generatingOnLoad,acceptsStop,stopFinishDelayMs,initialModel,shiftedEfforts,hideEffortLabels,nestedHistory,missingHistoryId,duplicateHistoryViews,destinationLocks,absentFamilies,duplicateEffortLabels,stopConfirmation,fixtureLanguage,localizedStopFallback,changedUrlAfterReload]);}
async function syncFixture(){if(loadedFixtureState!==fixtureState()){stopReloadAt=documentLoads+2;await surface.navigate(page.url());}}
async function executeFixture(driver:ChatGptBrowserDriver,execution:DriverExecution){await syncFixture();return driver.execute(execution);}
async function executeCoordinatorFixture(coordinator:SingleWindowCoordinator,request:ExecuteActionRequest){await syncFixture();return coordinator.execute(request);}

async function fixture(draft="",canSend=true,name=connector,quietMs=0,generating=false,stopWorks:boolean|number|"reload"=true,model={family:"6",value:4,locked:[] as number[]}) {
  // Isolate each scenario's navigation budget without freezing live deadlines.
  if(context!==browserContext)await context.close();
  context=await browserContext.browser()!.newContext({viewport:null});
  page=await context.newPage();
  await installFixtureRoutes();
  initialDraft=draft;ready=canSend;chipName=name;quietGenerationMs=quietMs;
  generatingOnLoad=generating;acceptsStop=stopWorks;stopFinishDelayMs=200;stopConfirmation=undefined;fixtureLanguage="en";localizedStopFallback=false;
  documentLoads=0;stopReloadAt=2;sessionRequests=0;sessionUnavailableBeforeLoad=0;changedAccountAfterReload=false;changedUrlAfterReload=false;
  initialModel=model;shiftedEfforts=false;hideEffortLabels=false;nestedHistory=false;missingHistoryId=false;duplicateHistoryViews=false;
  destinationLocks={};absentFamilies=[];duplicateEffortLabels=false;changeAccountAtRequest=Number.MAX_SAFE_INTEGER;
  await surface.navigate(url);
  return new ChatGptBrowserDriver(surface,build);
}

test.skipIf(!native)("pairing waits for enabled Send and binds to the new answer after history hydration",async()=>{
  const driver=await fixture();
  const events: DriverExecution["emit"] extends (event:infer T)=>void ? T[] : never = [];
  await executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:event=>events.push(event)});
  expect(await page.evaluate(()=> Number(document.body.dataset.sends))).toBe(1);
  expect(events.some(event=>event.type==="send_accepted")).toBeTrue();
  expect(events.at(-1)).toMatchObject({type:"submission_complete",canonicalUrl:url});
  expect((await driver.health()).status).toBe("idle");
  expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe("Medium");
  expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("Pro");
},15_000);

test.skipIf(!native)("successive browser sends reuse a healthy paired conversation",async()=>{
  const driver=await fixture();
  for(const [id,model,family] of [['reuse-first','gpt-6-astra','6'],['reuse-second','gpt-5.6-sol','5.6']] as const){
    const submitted={...offlineRequest(model,'max'),actionId:id,idempotencyKey:id};
    const events:Parameters<DriverExecution['emit']>[0][]=[];
    await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:event=>events.push(event)});
    expect(events).toContainEqual({type:'model_verified',model,effort:'max'});
    expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort])).toEqual([family,'Pro']);
    expect(documentLoads).toBe(1);
  }
  expect(documentLoads).toBe(1);expect(await page.locator('body').getAttribute('data-sends')).toBe('2');
},20_000);

test.skipIf(!native)("conversation navigation waits for the composer to hydrate before sending",async()=>{
  await fixture();
  await surface.navigate("https://chatgpt.com/c/previous-conversation");
  const delayed:SingleBrowserSurface={...surface,navigate:async destination=>{
    const opened=await surface.navigate(destination);
    await opened.evaluate(()=>{
      const form=document.querySelector('form[data-chatgpt-composer]')!;
      const next=form.nextSibling,parent=form.parentNode!;
      form.remove();setTimeout(()=>parent.insertBefore(form,next),600);
    });
    return opened;
  }};
  const driver=new ChatGptBrowserDriver(delayed,build),submitted=offlineRequest('gpt-6-astra','max');
  const loads=documentLoads;
  await driver.execute({request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
  expect(documentLoads).toBe(loads+1);
},15_000);

test.skipIf(!native)("a paired conversation still hydrating its composer is not needlessly reloaded",async()=>{
  const driver=await fixture(),submitted=offlineRequest('gpt-6-astra','max');
  await page.evaluate(()=>{
    const form=document.querySelector('form[data-chatgpt-composer]')!;
    const next=form.nextSibling,parent=form.parentNode!;
    form.remove();setTimeout(()=>parent.insertBefore(form,next),600);
  });
  const loads=documentLoads;
  await driver.execute({request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
  expect(documentLoads).toBe(loads);
},15_000);

test.skipIf(!native)("a signed-in conversation with no editor reports editor availability rather than sign-in",async()=>{
  await fixture();
  await surface.navigate("https://chatgpt.com/c/previous-conversation");
  const missing:SingleBrowserSurface={...surface,navigate:async destination=>{
    const opened=await surface.navigate(destination);
    await opened.locator('form[data-chatgpt-composer]').evaluate(form=>form.remove());
    return opened;
  }};
  const driver=new ChatGptBrowserDriver(missing,build),submitted=offlineRequest('gpt-6-astra','max');
  const error=await driver.execute({request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}}).catch(error=>error);
  expect(error).toMatchObject({code:'editor_unavailable',phase:'composer_lookup',sendAccepted:false});
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
},15_000);

test.skipIf(!native)("cancelling composer hydration stops without reloading or sending",async()=>{
  const driver=await fixture(),submitted=offlineRequest('gpt-6-astra','max');
  await page.locator('form[data-chatgpt-composer]').evaluate(form=>form.remove());
  const abort=new AbortController(),loads=documentLoads;
  const timer=setTimeout(()=>abort.abort(),200);
  try {
    const error=await driver.execute({request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:abort.signal,emit:()=>{}}).catch(error=>error);
    expect(error).toBeInstanceOf(Error);
    expect(abort.signal.aborted).toBeTrue();
    expect(documentLoads).toBe(loads);
    expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
  } finally {clearTimeout(timer);}
},5_000);

test.skipIf(!native)("editor rejection is a confirmed-unsent availability failure while edited drafts remain interference",async()=>{
  for(const mode of ["rejected","exception","edited"] as const){
    const driver=await fixture();
    const submitted=offlineRequest('gpt-6-astra','max'),text=renderActionPrompt(submitted.prompt);
    await page.evaluate(({mode,text})=>{
      const original=document.execCommand.bind(document);
      document.execCommand=(command,showUi,value)=>{
        if(command==='insertText'&&value===text){
          if(mode==='rejected')return false;
          if(mode==='exception')throw new Error('Editor detached during insertion');
          const accepted=original(command,showUi,value);
          document.querySelector('[contenteditable="true"]')!.lastElementChild!.append(' edited');
          return accepted;
        }
        return original(command,showUi,value);
      };
    },{mode,text});
    const coordinator=new SingleWindowCoordinator(driver,build);
    await executeCoordinatorFixture(coordinator,submitted);
    const events=[];for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
    expect(events.at(-1)).toMatchObject(mode==='edited'
      ? {type:'external_interference'}
      : {type:'failed',code:'editor_unavailable',message:'ChatGPT did not accept the message in its editor; nothing was sent'});
    expect((await coordinator.action(submitted.actionId))?.sendAccepted).toBeFalse();
    expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
    if(mode==='edited')expect(await composerText(await activeComposer(page))).toBe(text+' edited');
  }
},15_000);

test.skipIf(!native)("a nonempty draft is interference rather than a retryable insertion failure",async()=>{
  await fixture('User draft');
  const composer=await activeComposer(page);
  await expect(insertExactPrompt(composer,'Automation message')).rejects.toMatchObject({code:'external_interference'});
  expect(await composerText(composer)).toBe('User draft');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
});

test.skipIf(!native)("sending an existing conversation tolerates nested wrappers for one turn",async()=>{
  const driver=await fixture();nestedHistory=true;
  const submitted=offlineRequest('gpt-6-astra','max');
  const error=await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
  expect(error).toBeUndefined();expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
},15_000);

test.skipIf(!native)("sending counts duplicated history views as one logical turn",async()=>{
  const driver=await fixture();duplicateHistoryViews=true;
  const submitted=offlineRequest('gpt-6-astra','max');
  await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
},15_000);

test.skipIf(!native)("unsupported history markers report the cause before inserting any message",async()=>{
  const driver=await fixture();missingHistoryId=true;
  const submitted=offlineRequest('gpt-6-astra','max');
  const error=await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
  expect(error).toMatchObject({code:'selector_drift',message:'ChatGPT legacy turn has no stable data-turn-id'});
  expect(await composerText(await activeComposer(page))).toBe('');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
},15_000);

test.skipIf(!native)("Send readiness excludes the voice-bars icon until the control changes",async()=>{
  for(const provisionalTestId of [false,true]) {
    await fixture('Readiness fixture');
    await page.locator('form button[type="submit"]').evaluate((button,provisional)=>{
      const target=button as HTMLButtonElement;target.type='button';target.disabled=false;target.setAttribute('aria-label','Start Voice');
      if(provisional)target.dataset.testid='send-button';
      target.innerHTML='<svg><use href="/cdn/assets/icons.svg#voice-regular-20"></use></svg>';
      setTimeout(()=>{target.type='submit';target.dataset.testid='send-button';target.setAttribute('aria-label','Envoyer');target.innerHTML='<svg><use href="/cdn/assets/icons.svg#arrow-up-regular-20"></use></svg>';},300);
    },provisionalTestId);
    const button=await uniqueSendButton(await activeComposer(page),2_000);
    expect(await button.getAttribute('type')).toBe('submit');
    expect(await button.locator('use').getAttribute('href')).toContain('#arrow-up-');
  }
},15_000);

const offlineRequest = (model = "gpt-5.5-sol", effort = "low"): ExecuteActionRequest => ({
  ...request, model, effort, prompt:{kind:"offline_goal",goalRevision:1},
});

test.skipIf(!native)('a stale expired-session, sign-in or no-composer page reloads once before a single send',async()=>{
 for(const kind of ['expired','signed-out','missing-composer'] as const){
  const driver=await fixture();
  if(kind==='expired')await page.evaluate(()=>document.body.insertAdjacentHTML('beforeend','<div role="alert">Your session has expired</div>'));
  else {if(kind==='signed-out')sessionUnavailableBeforeLoad=documentLoads+1;await page.locator('form').evaluate(form=>form.remove());}
  const coordinator=new SingleWindowCoordinator(driver,build),loads=documentLoads;
  expect((await coordinator.health()).status).toBe(kind==='missing-composer'?'idle':'authentication_required');
  expect(documentLoads).toBe(loads);
  const submitted=offlineRequest('gpt-6-astra','max');
  await executeCoordinatorFixture(coordinator,submitted);
  const events=[];for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe('submission_complete');
  expect(documentLoads).toBe(loads+1);expect(page.url()).toBe(url);
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
 }
},45_000);

test.skipIf(!native)('authentication recovery refuses a changed account and preserves a pending materialization card',async()=>{
 for(const kind of ['account','approval'] as const){
  const driver=await fixture();
  await page.evaluate(()=>document.body.insertAdjacentHTML('beforeend','<div role="alert">Your session has expired</div>'));
  if(kind==='account')changedAccountAfterReload=true;
  else await page.evaluate(()=>document.body.insertAdjacentHTML('beforeend','<div data-codex-approval-surface="true">Pending materialization</div>'));
  const coordinator=new SingleWindowCoordinator(driver,build),loads=documentLoads,submitted=offlineRequest('gpt-6-astra','max');
  await executeCoordinatorFixture(coordinator,submitted);
  const events=[];for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe(kind==='account'?'authentication_required':'approval_required');
  expect(documentLoads).toBe(loads+(kind==='account'?1:0));
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
  expect(await composerText(await activeComposer(page))).toBe('');
 }
},15_000);
test.skipIf(!native)("pairing, messages, and new chats clear pre-existing drafts before sending",async()=>{
  const requests:ExecuteActionRequest[]=[request,offlineRequest(),{
    ...offlineRequest(),prompt:{kind:"offline_chat",batchId:"draft-test",messages:[{id:"message-one",markdown:"Continue the fixture task"}]},
  },{
    ...request,url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",
    prompt:{kind:"create_chat",reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null},
  }];
  for(const submitted of requests) {
    const driver=await fixture('Discard this existing draft');
    await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
    expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
    expect(await page.locator('body').getAttribute('data-sent-text')).not.toContain('Discard this existing draft');
    expect(await composerText(await activeComposer(page))).toBe('');
  }
},35_000);

test.skipIf(!native)("draft cleanup still requires the expected account and conversation",async()=>{
  for(const mismatch of ['account','conversation']) {
    const driver=await fixture('Keep this draft in the wrong destination');
    const submitted={...request,...(mismatch==='account'?{expectedAccountFingerprint:'another-account'}:{expectedUrl:'https://chatgpt.com/c/another-conversation'})};
    const error=await executeFixture(driver,{request:submitted,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
    expect(error).toMatchObject({code:mismatch==='account'?'authentication_required':'conversation_unavailable'});
    expect(await composerText(await activeComposer(page))).toBe('Keep this draft in the wrong destination');
    expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
  }
},15_000);

test.skipIf(!native)("whitespace-only drafts are removed from the submitted text",async()=>{
  for(const draft of [' \t ','   ','\n']) {
    const driver=await fixture(draft);
    await executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}});
    expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
    const sent=(await page.locator('body').getAttribute('data-sent-text'))!;
    // Chrome may insert one separator after the reselected connector chip.
    expect(sent.slice(connector.length).replace(/^ /,'')).toBe(prompt.replace(/\n/g,''));
  }
},25_000);

test.skipIf(!native)("model discovery preserves an unsent draft and waits for an empty composer",async()=>{
  const driver=await fixture('Keep this model-picker draft');
  const failure=await driver.capabilities().catch(error=>error);
  expect(failure.message).toContain('requires an empty composer');
  expect(failure.phase).toBe('capability_discovery');
  expect(await composerText(await activeComposer(page))).toBe('Keep this model-picker draft');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
},20_000);

test.skipIf(!native)("new-chat preparation clears the draft on the new chat surface",async()=>{
  const driver=await fixture('An old model-picker draft');
  const catalog=await driver.prepareNewChat(null);
  expect(catalog.models.length).toBeGreaterThan(0);
  expect(await composerText(await activeComposer(page))).toBe('');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
},20_000);

test.skipIf(!native)("a cold-cache send navigates before touching models and selects only the destination family",async()=>{
  await fixture();
  await page.goto("https://chatgpt.com/c/unrelated-conversation");
  initialModel={family:"6",value:4,locked:[]};
  let source:unknown;
  const targetSurface={...surface, navigate:async(destination:string)=>{
    source=await page.evaluate(()=>({family:document.body.dataset.family, moves:document.body.dataset.effortChanges, families:document.body.dataset.familyChanges}));
    return surface.navigate(destination);
  }};
  const driver=new ChatGptBrowserDriver(targetSurface,build);
  driver.capabilities=async()=>{throw new Error("A send must not refresh the full catalog");};
  const submitted=offlineRequest();const events:Parameters<DriverExecution['emit']>[0][]=[];
  await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:event=>events.push(event)});
  expect(source).toEqual({family:"6",moves:"0",families:""});
  expect(page.url()).toBe(url);
  expect(await page.evaluate(()=>document.body.dataset.familyChanges)).toBe("5.5,");
  expect(await page.evaluate(()=>document.body.dataset.effortChanges)).toBe("3");
  expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort])).toEqual(["5.5","Instant"]);
  expect(events.map(event=>event.type)).toEqual(["navigating","composer_ready","model_verified","connector_verified","send_accepted","submission_complete"]);
},15_000);

test.skipIf(!native)("an already-selected request does not cycle any effort or model options",async()=>{
  const driver=await fixture("",true,connector,0,false,true,{family:"6",value:4,locked:[]});
  driver.capabilities=async()=>{throw new Error("Unexpected catalog refresh");};
  const submitted=offlineRequest("gpt-6-astra","max");
  await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(await page.evaluate(()=>document.body.dataset.effortChanges)).toBe("0");
  expect(await page.evaluate(()=>document.body.dataset.familyChanges)).toBe("");
  expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe("Pro");
},15_000);

test.skipIf(!native)("a changed account prevents selection and Send even when fallback would be available",async()=>{
  const driver=await fixture("",true,connector,0,false,true,{family:"6",value:4,locked:[2]});
  const submitted={...offlineRequest("gpt-6-sol","high"),expectedAccountFingerprint:"another-account"};
  let failure:unknown;
  try {await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});}catch(error){failure=error;}
  expect(failure).toBeInstanceOf(Error);
  expect(await page.evaluate(()=>document.body.dataset.sends)).toBe("0");
  expect(await page.evaluate(()=>document.body.dataset.effortChanges)).toBe("0");
  expect(await composerText(await activeComposer(page))).toBe("");
},15_000);

test.skipIf(!native)("locked preferred models follow the fallback order and try the preference again next turn",async()=>{
  const driver=await fixture(),coordinator=new SingleWindowCoordinator(driver,build);
  destinationLocks={'6':[4]};
  const preferred=offlineRequest('gpt-6-astra','max');await executeCoordinatorFixture(coordinator,preferred);
  const events=[];for await(const event of coordinator.events(preferred.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe('submission_complete');
  expect((await coordinator.action(preferred.actionId))?.selected).toEqual({model:'gpt-5.6-sol',effort:'max'});
  expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort,document.body.dataset.sends])).toEqual(['5.6','Pro','1']);
  expect(preferred.model).toBe('gpt-6-astra');expect(preferred.effort).toBe('max');
  destinationLocks={};const next={...preferred,actionId:'preferred-restored',idempotencyKey:'preferred-restored-key'};
  await executeCoordinatorFixture(coordinator,next);for await(const event of coordinator.events(next.actionId,0)){}
  expect((await coordinator.action(next.actionId))?.selected).toEqual({model:'gpt-6-astra',effort:'max'});
  expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe('Pro');await coordinator.close();
},20_000);

test.skipIf(!native)("goal continuations advance to the next available model on later turns",async()=>{
  const driver=await fixture(),coordinator=new SingleWindowCoordinator(driver,build);
  const preferred=offlineRequest('gpt-6-astra','max');
  for(const turn of [1,2]) {
    destinationLocks=turn===1?{'6':[4]}:{'6':[4],'5.6':[4]};
    const next:ExecuteActionRequest={...preferred,actionId:`goal-turn-${turn}`,idempotencyKey:`goal-turn-${turn}-key`,
      prompt:turn===1?{kind:'offline_goal',goalRevision:1}:{kind:'offline_combined',goalRevision:1,batchId:'goal-follow-up',messages:[{id:'follow-up',markdown:'Continue'}]}};
    await executeCoordinatorFixture(coordinator,next);
    const events=[];for await(const event of coordinator.events(next.actionId,0))events.push(event);
    expect(events.at(-1)?.type).toBe('submission_complete');
    expect(events.filter(event=>event.type==='send_accepted')).toHaveLength(1);
    expect((await coordinator.action(next.actionId))?.selected).toEqual(turn===1
      ?{model:'gpt-5.6-sol',effort:'max'}:{model:'gpt-6-sol',effort:'xhigh'});
    expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort,document.body.dataset.sends]))
      .toEqual(turn===1?['5.6','Pro','1']:['6','Extra High','1']);
    expect((await coordinator.health()).activeActionId).toBeNull();
    expect(next.model).toBe('gpt-6-astra');expect(next.effort).toBe('max');
  }
  await coordinator.close();
},20_000);

test.skipIf(!native)("fallback skips missing families and locked efforts in the requested order",async()=>{
  for(const locked of [[4],[4,3]]) {
    const driver=await fixture();destinationLocks={'6':locked};absentFamilies=['5.6'];
    const submitted=offlineRequest('gpt-6-astra','max'),events:Parameters<DriverExecution['emit']>[0][]=[];
    await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:event=>events.push(event)});
    expect(events.find(event=>event.type==='model_verified')).toMatchObject({model:'gpt-6-sol',effort:locked.length===1?'xhigh':'high'});
    expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort,document.body.dataset.sends])).toEqual(['6',locked.length===1?'Extra High':'High','1']);
  }
},20_000);

test.skipIf(!native)("exhausted fallbacks, ambiguous picker state, and changed accounts never send",async()=>{
  for(const mode of ['exhausted','ambiguous','account']) {
    const driver=await fixture();destinationLocks=mode==='exhausted'?{'6':[0,1,2,3,4],'5.6':[0,1,2,3,4],'5.5':[0,1,2,3,4]}:mode==='account'?{'6':[4]}:{};
    duplicateEffortLabels=mode==='ambiguous';if(mode==='account')changeAccountAtRequest=2;
    const submitted=offlineRequest(mode==='ambiguous'?'gpt-6-sol':'gpt-6-astra',mode==='ambiguous'?'high':'max');
    let failure:unknown;try{await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});}catch(error){failure=error;}
    expect(failure).toBeInstanceOf(Error);expect(await page.evaluate(()=>document.body.dataset.sends)).toBe('0');
    expect(await composerText(await activeComposer(page))).toBe('');
    if(mode==='exhausted')expect((failure as Error).message).toContain('all supported fallbacks');
    if(mode==='ambiguous')expect(await page.evaluate(()=>document.body.dataset.familyChanges)).toBe('');
    if(mode==='account')expect((failure as any).code).toBe('authentication_required');
  }
},25_000);

test.skipIf(!native)("a stale catalog index does not override the destination's native effort labels",async()=>{
  const driver=await fixture();
  await driver.capabilities();
  // The old catalog places High at index 3. The destination now exposes it at index 2.
  shiftedEfforts=true;
  await page.goto("https://chatgpt.com/c/unrelated-conversation");
  const submitted=offlineRequest("gpt-5.5-sol","high");
  await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort])).toEqual(["5.5","High"]);
  expect(await page.locator('[role="slider"]').getAttribute('aria-valuenow')).toBe("9");
},15_000);

test.skipIf(!native)("unlabeled destination ticks use a verified cache hint or work without a cache",async()=>{
  for(const warm of [false,true]) {
    const driver=await fixture();
    if(warm) await driver.capabilities();
    hideEffortLabels=true;
    await page.goto("https://chatgpt.com/c/unrelated-conversation");
    const submitted=offlineRequest("gpt-6-sol",warm?"high":"low");
    await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}});
    expect(await page.evaluate(()=>document.body.dataset.familyChanges)).toBe("");
    expect(await page.evaluate(()=>document.body.dataset.effortChanges)).toBe(warm?"2":"4");
    expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe(warm?"High":"Instant");
  }
},20_000);

test.skipIf(!native)("a destination effort locked after selection prevents Send",async()=>{
  const driver=await fixture("",false);
  const submitted=offlineRequest("gpt-6-sol","high");
  const run=executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
  await page.waitForFunction(()=>document.querySelector('[contenteditable="true"]')!.textContent!.includes('goal'));
  await page.locator('[data-selected="true"]').evaluate(element=>element.setAttribute('data-locked','true'));
  await page.locator('button[type="submit"]').evaluate(element=>(element as HTMLButtonElement).disabled=false);
  const failure=await run;
  expect(failure).toBeInstanceOf(Error);
  expect(await page.evaluate(()=>document.body.dataset.sends)).toBe("0");
},15_000);

test.skipIf(!native)("pairing uses Medium for each model family and restores the exact original effort",async()=>{
  for(const model of [{family:"6",value:4,locked:[]},{family:"5.6",value:2,locked:[]},{family:"5.5",value:11,locked:[]}]) {
    const driver=await fixture("",true,connector,31_000,false,true,model);
    const started=Date.now();
    await executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}});
    expect(Date.now()-started).toBeLessThan(10_000);
    expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe("Medium");
    expect(await page.evaluate(()=>document.body.dataset.sentFamily)).toBe(model.family);
    expect(await page.locator('[role="slider"]').getAttribute('aria-valuenow')).toBe(String(model.value));
    expect(await page.evaluate(()=>document.body.dataset.family)).toBe(model.family);
    expect(await page.locator('#effort').getAttribute('aria-expanded')).toBe('false');
    expect(await page.locator('[data-testid="stop-button"]').count()).toBe(1);
    expect(request.model).toBeNull();expect(request.effort).toBeNull();
  }
},30_000);

test.skipIf(!native)("already-Medium pairing does not move the effort slider",async()=>{
  const driver=await fixture("",true,connector,0,false,true,{family:"6",value:1,locked:[]});
  await executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}});
  expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe("Medium");
  expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("Medium");
  expect(await page.evaluate(()=>document.body.dataset.effortChanges)).toBe("0");
},15_000);

test.skipIf(!native)("unavailable Medium preserves the chosen settings without sending a pairing turn",async()=>{
  const driver=await fixture("",true,connector,0,false,true,{family:"6",value:4,locked:[1]});
  let failure:unknown;
  try { await executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}}); } catch(error) {failure=error;}
  expect(failure).toBeInstanceOf(Error);
  expect((failure as Error).message).toContain("Medium mode is unavailable");
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(0);
  expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("Pro");
  expect(await page.locator('#effort').getAttribute('aria-expanded')).toBe('false');
},15_000);

test.skipIf(!native)("external model or effort changes during pairing are preserved and prevent Send",async()=>{
  for(const changed of ["family","effort"]) {
    const driver=await fixture("",false);
    const run=executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
    await page.waitForFunction(()=>document.querySelector('[contenteditable="true"]')!.textContent!.includes('automation_verify_pairing'));
    await page.locator('#effort').click();
    if(changed==='family') await page.locator('[data-family="5.6"]').click();
    for(let index=0;index<(changed==='family'?2:1);index++) await page.locator('#range').press(changed==='family'?'ArrowLeft':'ArrowRight');
    await page.keyboard.press('Escape');
    await page.locator('button[type="submit"]').evaluate(element=>(element as HTMLButtonElement).disabled=false);
    const error=await run;expect(error).toBeInstanceOf(Error);
    expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(0);
    expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("High");
    expect(await page.evaluate(()=>document.body.dataset.family)).toBe(changed==='family'?'5.6':'6');
  }
},20_000);

test.skipIf(!native)("automatic pairing uses Medium before applying the configured conversation model",async()=>{
  const driver=await fixture("",true,connector,31_000,false,true,{family:"5.6",value:2,locked:[]});
  const create:ExecuteActionRequest={...request,url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",model:"gpt-6-astra",effort:"max",prompt:{kind:"create_chat",reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null}};
  const events:Parameters<DriverExecution['emit']>[0][]=[];
  const coordinator=new SingleWindowCoordinator(driver,build);
  await executeCoordinatorFixture(coordinator,create);for await(const event of coordinator.events(create.actionId,0))events.push(event);
  expect(await page.evaluate(()=>document.body.dataset.sentEffort)).toBe("Medium");
  expect(await page.evaluate(()=>document.body.dataset.sentFamily)).toBe("5.6");
  expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("Pro");
  expect(await page.evaluate(()=>document.body.dataset.family)).toBe("6");
  expect(create.model).toBe("gpt-6-astra");expect(create.effort).toBe("max");
  expect(events.findIndex(event=>event.type==='model_verified')).toBeGreaterThan(events.findIndex(event=>event.type==='send_accepted'));
  expect(events.at(-1)).toMatchObject({type:'submission_complete',canonicalUrl:'https://chatgpt.com/c/new-created-fixture'});
  expect(await page.locator('[data-testid="stop-button"]').count()).toBe(1);
  expect((await coordinator.action(create.actionId))?.selected).toEqual({model:"gpt-6-astra",effort:"max"});
},30_000);

test.skipIf(!native)("cancelled automatic pairing preserves the original composer and configured preferences",async()=>{
  const driver=await fixture("",false,connector,0,false,true,{family:"5.6",value:2,locked:[]});
  const create:ExecuteActionRequest={...request,url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",model:"gpt-6-astra",effort:"max",prompt:{kind:"create_chat",reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null}};
  const controller=new AbortController();
  const run=executeFixture(driver,{request:create,renderedPrompt:renderActionPrompt(create.prompt),signal:controller.signal,emit:()=>{}}).then(()=>undefined,error=>error);
  await page.waitForFunction(()=>document.querySelector('[contenteditable="true"]')!.textContent!.includes('continue_task'));
  controller.abort();expect(await run).toBeInstanceOf(Error);
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(0);
  expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("High");
  expect(await page.evaluate(()=>document.body.dataset.family)).toBe("5.6");
  expect(create.model).toBe("gpt-6-astra");expect(create.effort).toBe("max");
  expect(await composerText(await activeComposer(page))).toBe('');
},15_000);

test.skipIf(!native)("automatic pairing cannot fall back to a configured thinking request when Medium is locked",async()=>{
  const driver=await fixture("",true,connector,0,false,true,{family:"5.6",value:2,locked:[1]});
  const create:ExecuteActionRequest={...request,url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",model:"gpt-6-astra",effort:"max",prompt:{kind:"create_chat",reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null}};
  const failure=await executeFixture(driver,{request:create,renderedPrompt:renderActionPrompt(create.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
  expect(failure).toMatchObject({code:"model_unavailable",sendAccepted:false});
  expect(await page.evaluate(()=>document.body.dataset.sends)).toBe("0");
  expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort])).toEqual(["5.6","High"]);
  expect(create.model).toBe("gpt-6-astra");expect(create.effort).toBe("max");
},15_000);

test.skipIf(!native)("existing-chat pairing ignores configured model selection and restores its original choice",async()=>{
  const driver=await fixture("",true,connector,31_000,false,true,{family:"5.6",value:2,locked:[]});
  const configured={...request,model:"gpt-6-astra",effort:"max"};
  await executeFixture(driver,{request:configured,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}});
  expect(await page.evaluate(()=>[document.body.dataset.sentFamily,document.body.dataset.sentEffort])).toEqual(["5.6","Medium"]);
  expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort])).toEqual(["5.6","High"]);
  expect(configured.model).toBe("gpt-6-astra");expect(configured.effort).toBe("max");
},15_000);

test.skipIf(!native)("an aborted pre-Send wait clears only the unchanged owned prompt",async()=>{
  for (const edited of [false,true]) {
    const driver=await fixture("",false);const controller=new AbortController();
    const run=executeFixture(driver,{request,renderedPrompt:prompt,signal:controller.signal,emit:()=>{}}).then(()=>undefined,error=>error);
    await page.waitForFunction(()=>document.querySelector('[contenteditable="true"]')!.textContent!.includes('automation_verify_pairing'));
    if (edited) await page.locator('[contenteditable="true"]').pressSequentially(" user edit");
    const beforeAbort=await composerText(await activeComposer(page));
    controller.abort();const failure=await run;
    expect(failure).toBeDefined();
    expect(await page.evaluate(()=> Number(document.body.dataset.sends))).toBe(0);
    const draft=await composerText(await activeComposer(page));
    expect(draft).toBe(edited?beforeAbort:"");
    expect((await driver.health()).status).toBe("idle");
    expect(await page.evaluate(()=>document.body.dataset.effort)).toBe("Pro");
  }
},15_000);

test.skipIf(!native)("Replace pairing recovers an old verification draft without resubmitting its reference",async()=>{
  const driver=await fixture(oldPrompt);const coordinator=new SingleWindowCoordinator(driver,build);
  expect((await coordinator.health()).status).toBe("idle");
  expect((await coordinator.health()).activeActionId).toBeNull();
  await executeCoordinatorFixture(coordinator,request);
  const events=[];for await (const event of coordinator.events(request.actionId,0)) events.push(event);
  expect(await page.evaluate(()=> Number(document.body.dataset.sends))).toBe(1);
  expect(await page.evaluate(()=> document.body.dataset.sentText)).toContain(newReference);
  expect(await page.evaluate(()=> document.body.dataset.sentText)).not.toContain("A".repeat(43));
  expect(events.at(-1)?.type).toBe("submission_complete");
  expect((await coordinator.health()).activeActionId).toBeNull();
},15_000);

test.skipIf(!native)("an exact connector menu choice binds an unrelated internal chip name to its app identity",async()=>{
  const driver=await fixture(oldPrompt,true,"internal-name-unrelated-to-display-label");
  const coordinator=new SingleWindowCoordinator(driver,build);
  await executeCoordinatorFixture(coordinator,request);
  const events=[];for await (const event of coordinator.events(request.actionId,0)) events.push(event);
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(1);
  expect(events.at(-1)?.type).toBe("submission_complete");
},15_000);

test.skipIf(!native)("a slow connector menu loads without clearing and retriggering the mention",async()=>{
  const driver=await fixture("Discard this existing draft");
  await page.evaluate(()=>{
    const editor=document.querySelector('[contenteditable="true"]')!;
    const mentions=document.querySelector('[data-mention-list-scroll-area]') as HTMLElement;
    let timer:ReturnType<typeof setTimeout>;
    let typingMention=false;
    document.body.dataset.connectorAttempts="0";
    editor.addEventListener('input',event=>{
      clearTimeout(timer);
      const querying=editor.textContent!.startsWith('@');
      if(querying&&!typingMention)document.body.dataset.connectorAttempts=String(Number(document.body.dataset.connectorAttempts)+1);
      typingMention=querying;
      if(!querying)return;
      event.stopImmediatePropagation();
      mentions.hidden=true;
      timer=setTimeout(()=>{mentions.hidden=false;},4_000);
    },true);
  });
  const coordinator=new SingleWindowCoordinator(driver,build);
  await executeCoordinatorFixture(coordinator,offlineRequest('gpt-6-astra','max'));
  const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe('submission_complete');
  expect(await page.locator('body').getAttribute('data-connector-attempts')).toBe('1');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
},15_000);

test.skipIf(!native)("a missing connector sends the message once without a mention or a page reload",async()=>{
  const driver=await fixture("Discard this existing draft");
  await page.locator('[data-mention-list-scroll-area]').evaluate(element=>element.remove());
  const loads=documentLoads;
  const coordinator=new SingleWindowCoordinator(driver,build);
  const submitted={...offlineRequest('gpt-6-astra','max'),prompt:{kind:'offline_chat' as const,batchId:'missing-connector',messages:[{id:'message-without-mention',markdown:'Proceed without a mention.'}]}};
  await executeCoordinatorFixture(coordinator,submitted);
  const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
  expect(events.at(-1)).toMatchObject({type:'submission_complete'});
  expect(events.some(event=>event.type==='connector_verified')).toBeFalse();
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
  expect(await page.locator('body').getAttribute('data-sent-text')).toContain('Proceed without a mention.');
  expect(await page.locator('body').getAttribute('data-sent-text')).not.toContain(`@${connector}`);
  expect(await page.locator('[app-mention-path]').count()).toBe(0);
  expect(documentLoads).toBe(loads);
},35_000);

test.skipIf(!native)("a connector app identity changed after selection is rejected before Send",async()=>{
  const driver=await fixture("",false,"internal-name-unrelated-to-display-label");
  const run=executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:()=>{}})
    .then(()=>undefined,error=>error);
  await page.waitForFunction(()=>document.querySelector('[contenteditable="true"]')!.textContent!.includes('automation_verify_pairing'));
  await page.locator('[app-mention-path]').evaluate(element=>element.setAttribute('app-mention-path','app://different-app'));
  await page.locator('button[type="submit"]').evaluate(element=>(element as HTMLButtonElement).disabled=false);
  const failure=await run;
  expect((failure as Error).message).toContain("changed the selected connector");
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(0);
  expect(await composerText(await activeComposer(page))).toBe(prompt);
},15_000);

test.skipIf(!native)("pairing preserves edited drafts, different conversations, missing connectors, and running generations",async()=>{
  for (const kind of ["user","edited","other-conversation","missing-connector","generating","approval"]) {
    const draft=kind==="user"?"My own draft":kind==="edited"?oldPrompt+" edited":oldPrompt;
    const driver=await fixture(draft);
    if (kind==="missing-connector") await page.locator('[app-mention-path]').evaluate(element=>element.remove());
    if (kind==="generating") await page.evaluate(()=>document.body.insertAdjacentHTML('beforeend','<button data-testid="stop-button">Stop</button>'));
    if (kind==="approval") await page.evaluate(()=>document.body.insertAdjacentHTML('beforeend','<div role="dialog">Allow ChatGPT to use Fixture connector?</div>'));
    const submitted=kind==="other-conversation"?{...request,expectedUrl:"https://chatgpt.com/c/other"}:request;
    expect(await driver.replacePairingDraft(submitted)).toBeFalse();
    expect(await composerText(await activeComposer(page))).toBe(draft);
    expect(await page.evaluate(()=> Number(document.body.dataset.sends))).toBe(0);
  }
},15_000);

test.skipIf(!native)("Send readiness has a deadline and rejects multiple owned submitters",async()=>{
  await fixture("draft",false);const composer=await activeComposer(page);
  let failure:unknown;try {await uniqueSendButton(composer,100);}catch(error){failure=error;}
  expect((failure as Error).message).toContain("did not become ready");
  await page.evaluate(()=>document.querySelector('form')!.insertAdjacentHTML('beforeend','<button type="submit">Other submit</button>'));
  try {await uniqueSendButton(composer,100);}catch(error){failure=error;}
  expect((failure as Error).message).toContain("one visible Send control");
},5_000);

test.skipIf(!native)("composer text preserves native paragraph, hard-break and blank-line boundaries",async()=>{
  await fixture();
  const composer=await activeComposer(page);
  for (const placeholder of ['<br>','<br class="ProseMirror-trailingBreak">']) {
    await composer.evaluate((element,breakMarkup)=>{
      element.innerHTML='<p><span app-mention-path="app://fixture" contenteditable="false">internal-name</span> line  one</p><p>line two<br>hard break</p><p>'+breakMarkup+'</p><p>last line</p>';
    },placeholder);
    expect(await composerText(composer)).toBe("line  one\nline two\nhard break\n\nlast line");
  }
},5_000);

test.skipIf(!native)("submission holds the window for five seconds so a slow request finishes before the next navigation",async()=>{
  const driver=await fixture("",true,connector,31_000);
  const nextUrl="https://chatgpt.com/c/another-pair-fixture";
  const delayedSubmit="https://chatgpt.com/fixture-delayed-submit";
  await page.route(delayedSubmit,async route=>{
    await new Promise(resolve=>setTimeout(resolve,4_000));
    await route.fulfill({json:{accepted:true}});
  });
  await page.evaluate(()=>document.querySelector('form')!.addEventListener('submit',()=>{
    void fetch('/fixture-delayed-submit').then(()=>{document.body.dataset.requestFinished='true';});
  }));
  const coordinator=new SingleWindowCoordinator(driver,build);
  const began=Date.now();
  await executeCoordinatorFixture(coordinator,request);
  await page.waitForFunction(()=>document.body.dataset.sends==='1');
  const sentAt=await page.evaluate(()=>Number(document.body.dataset.sentAt));
  expect((await coordinator.health()).activeActionId).toBe(request.actionId);
  expect((await coordinator.health()).status).toBe('busy');
  await coordinator.reveal(nextUrl);
  expect(page.url()).toBe(url);
  const events=[];for await(const event of coordinator.events(request.actionId,0)) events.push(event);
  expect(Date.now()-sentAt).toBeGreaterThanOrEqual(5_000);
  expect(Date.now()-began).toBeLessThan(10_000);
  expect(await page.locator('body').getAttribute('data-request-finished')).toBe('true');
  expect(events.some(event=>["status_delta","commentary_delta","answer_delta","generation_complete"].includes(event.type))).toBeFalse();
  expect(events.at(-1)?.type).toBe("submission_complete");
  expect(await page.locator('[data-testid="stop-button"]').count()).toBe(1);
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(1);
  expect((await coordinator.health()).activeActionId).toBeNull();
  expect((await coordinator.health()).status).toBe("idle");
  const next={...request,actionId:"next-pair",idempotencyKey:"next-pair-key",url:nextUrl,expectedUrl:nextUrl};
  await executeCoordinatorFixture(coordinator,next);
  const nextEvents=[];for await(const event of coordinator.events(next.actionId,0)) nextEvents.push(event);
  expect(nextEvents.at(-1)?.type).toBe("submission_complete");
  expect(page.url()).toBe(nextUrl);
  await page.unroute(delayedSubmit);
},20_000);

test.skipIf(!native)("cancellation after Send keeps browser ownership through the five-second request settling window",async()=>{
  const driver=await fixture();
  const coordinator=new SingleWindowCoordinator(driver,build),submitted=offlineRequest('gpt-6-astra','max');
  await executeCoordinatorFixture(coordinator,submitted);
  for await(const event of coordinator.events(submitted.actionId,0))if(event.type==='send_accepted')break;
  const sentAt=await page.evaluate(()=>Number(document.body.dataset.sentAt));
  expect((await coordinator.health()).activeActionId).toBe(submitted.actionId);
  await coordinator.cancel({actionId:submitted.actionId,idempotencyKey:'cancel-after-submit'});
  expect(Date.now()-sentAt).toBeGreaterThanOrEqual(5_000);
  expect((await coordinator.action(submitted.actionId))?.sendAccepted).toBeTrue();
  expect((await coordinator.health()).activeActionId).toBeNull();
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
  expect(page.url()).toBe(url);expect(documentLoads).toBe(1);
},15_000);

test.skipIf(!native)("offline messages and goals stop lingering generation before Send and then release the queue",async()=>{
  const prompts:ExecuteActionRequest["prompt"][]=[
    {kind:"offline_chat",batchId:"offline-batch",messages:[{id:"offline-message",markdown:"Continue from here"}]},
    {kind:"offline_goal",goalRevision:1},
    {kind:"offline_combined",batchId:"combined-batch",messages:[{id:"combined-message",markdown:"An additional instruction"}],goalRevision:1},
  ];
  for (const actionPrompt of prompts) {
    const driver=await fixture("",true,connector,31_000,true);
    const coordinator=new SingleWindowCoordinator(driver,build);
    const offline={...request,prompt:actionPrompt};
    await executeCoordinatorFixture(coordinator,offline);
    const events=[];for await(const event of coordinator.events(offline.actionId,0)) events.push(event);
    expect(events.at(-1)).toMatchObject({type:"submission_complete"});
    expect(await page.evaluate(()=>document.body.dataset.order)).toBe("stop,stopped,send,");
    expect(await page.evaluate(()=>Number(document.body.dataset.stops))).toBe(1);
    expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(1);
    expect(await page.locator('[data-testid="stop-button"]').count()).toBe(1);
    expect(events.at(-1)?.type).toBe("submission_complete");
    expect((await coordinator.health()).activeActionId).toBeNull();
  }
},25_000);

test.skipIf(!native)("offline Stop confirms an optional localized dialog before sending once",async()=>{
  const prompts:ExecuteActionRequest["prompt"][]=[
    {kind:"offline_chat",batchId:"dialog-batch",messages:[{id:"dialog-message",markdown:"Continue"}]},
    {kind:"offline_goal",goalRevision:1},
    {kind:"offline_combined",batchId:"dialog-combined",messages:[{id:"combined-dialog-message",markdown:"Continue"}],goalRevision:1},
  ];
  for(const [index,actionPrompt] of prompts.entries()) {
    const driver=await fixture("",true,connector,0,true);
    stopConfirmation={delayMs:index===1?350:0};
    const offline:ExecuteActionRequest={...request,prompt:actionPrompt};
    await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}});
    expect(await page.locator("body").getAttribute("data-order")).toBe("stop,confirm,stopped,send,");
    expect(await page.locator("body").getAttribute("data-confirmations")).toBe("1");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
    expect(await page.locator('[role="dialog"]').count()).toBe(0);
    expect(documentLoads).toBe(2);
  }
},30_000);

test.skipIf(!native)("offline Stop resumes the captured already-open confirmation before sending",async()=>{
  for(const scenario of [{frame:undefined,captured:true,draft:'Existing draft'},{frame:'srcdoc',captured:true,draft:''},{frame:undefined,captured:false,draft:''},{frame:'nested',captured:false,draft:''}] as const){
    const driver=await fixture(scenario.draft,true,connector,0,true);
    fixtureLanguage=scenario.frame==='nested'?'it':'en';
    stopConfirmation={preexisting:true,captured:scenario.captured,frame:scenario.frame};stopFinishDelayMs=1_200;
    await syncFixture();const loads=documentLoads;
    const offline=offlineRequest();
    await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:AbortSignal.timeout(12_000),emit:()=>{}});
    expect(await page.locator('body').getAttribute('data-order')).toBe('confirm,stopped,send,');
    expect(await page.locator('body').getAttribute('data-stops')).toBe('0');
    expect(await page.locator('body').getAttribute('data-confirmations')).toBe('1');
    expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
    if(scenario.draft)expect(await page.locator('body').getAttribute('data-draft-on-confirmation')).toContain(scenario.draft);
    const times=await page.evaluate(()=>({confirmed:Number(document.body.dataset.confirmedAt),stopped:Number(document.body.dataset.stoppedAt),sent:Number(document.body.dataset.sentAt)}));
    expect(times.stopped-times.confirmed).toBeGreaterThanOrEqual(1_200);
    expect(times.sent).toBeGreaterThanOrEqual(times.stopped);
    expect(documentLoads).toBe(loads);
  }
},45_000);

test.skipIf(!native)("offline Stop never confirms unrelated or ambiguous dialogs",async()=>{
  for(const scenario of [{unrelated:true,preexisting:true},{unrelated:true},{duplicateSubmit:true}] as const) {
    const driver=await fixture("",true,connector,0,true);
    stopConfirmation=scenario;
    const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
    const failure=await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
    expect(failure).toMatchObject({code:"external_interference"});
    expect(await page.locator("body").getAttribute("data-confirmations")).toBe("0");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
    expect(await page.locator("body").getAttribute("data-stops")).toBe('preexisting' in scenario?"0":"1");
    expect(documentLoads).toBe(2);
  }
},15_000);

test.skipIf(!native)("late Stop dialog during message preparation is confirmed before touching the blocked editor",async()=>{
  const driver=await fixture();
  stopConfirmation={captured:true,afterModelSelection:true,trapFocus:true};
  stopFinishDelayMs=1_200;
  await syncFixture();const loads=documentLoads;
  const offline=offlineRequest('gpt-6-sol','high');
  await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:AbortSignal.timeout(20_000),emit:()=>{}});
  expect(await page.locator('body').getAttribute('data-order')).toBe('confirm,stopped,send,');
  expect(await page.locator('body').getAttribute('data-stops')).toBe('0');
  expect(await page.locator('body').getAttribute('data-confirmations')).toBe('1');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
  expect(await page.locator('[role="dialog"]').count()).toBe(0);
  expect(documentLoads).toBe(loads);
},25_000);

test.skipIf(!native)("offline Stop confirms dialogs in ChatGPT frames without reloading",async()=>{
  for(const frame of ["url","srcdoc","nested"] as const){
    const driver=await fixture("",true,connector,0,true);
    fixtureLanguage="it";stopConfirmation={frame,delayMs:100};
    await syncFixture();const loads=documentLoads;
    const offline=offlineRequest();
    await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:AbortSignal.timeout(12_000),emit:()=>{}});
    expect(await page.locator("body").getAttribute("data-order")).toBe("stop,confirm,stopped,send,");
    expect(await page.locator("body").getAttribute("data-confirmations")).toBe("1");
    expect(await page.locator("body").getAttribute("data-stops")).toBe("1");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
    expect(documentLoads).toBe(loads);
  }
},40_000);

test.skipIf(!native)("offline Stop rejects unrelated or ambiguous iframe dialogs without reloading",async()=>{
  for(const scenario of [{unrelated:true,preexisting:true},{unrelated:true},{duplicateSubmit:true}] as const){
    const driver=await fixture("",true,connector,0,true);
    stopConfirmation={frame:"srcdoc",...scenario};
    const offline=offlineRequest();
    const failure=await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:AbortSignal.timeout(5_000),emit:()=>{}}).then(()=>undefined,error=>error);
    expect(failure).toMatchObject({code:"external_interference"});
    expect(await page.locator("body").getAttribute("data-confirmations")).toBe("0");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
    expect(documentLoads).toBe(2);
  }
},15_000);

test.skipIf(!native)("offline Stop never confirms hidden or foreign iframe copies",async()=>{
  for(const frame of ["hidden","foreign"] as const){
    const driver=await fixture("",true,connector,0,true);
    stopConfirmation={frame};await syncFixture();const loads=documentLoads;
    const controller=new AbortController(),offline=offlineRequest();
    const running=executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:controller.signal,emit:()=>{}}).then(()=>undefined,error=>error);
    await page.locator('[data-stop-dialog-frame]').waitFor({state:"attached",timeout:5_000});
    const child=await page.locator('[data-stop-dialog-frame]').elementHandle().then(handle=>handle!.contentFrame());
    await child!.locator('[role="dialog"]').waitFor({state:"attached",timeout:5_000});
    controller.abort();expect(await running).toBeDefined();
    expect(await page.locator("body").getAttribute("data-confirmations")).toBe("0");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
    expect(documentLoads).toBe(loads);
  }
},15_000);

test.skipIf(!native)("offline Stop rechecks account and conversation before confirming",async()=>{
  for(const mismatch of ["account","conversation"]) {
    const driver=await fixture("",true,connector,0,true);
    stopConfirmation={changeUrl:mismatch==="conversation"};
    if(mismatch==="account")changeAccountAtRequest=3;
    const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
    const failure=await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
    expect(failure).toMatchObject({code:mismatch==="account"?"authentication_required":"conversation_unavailable"});
    expect(await page.locator("body").getAttribute("data-confirmations")).toBe("0");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
    expect(documentLoads).toBe(2);
  }
},15_000);

test.skipIf(!native)("an unresponsive Stop confirmation is bounded and does not resubmit",async()=>{
  const driver=await fixture("",true,connector,0,true);
  stopConfirmation={persists:true};
  const coordinator=new SingleWindowCoordinator(driver,build);
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  const began=Date.now();
  await executeCoordinatorFixture(coordinator,offline);
  const events=[];for await(const event of coordinator.events(offline.actionId,0))events.push(event);
  expect(Date.now()-began).toBeGreaterThanOrEqual(20_000);expect(Date.now()-began).toBeLessThan(28_000);
  expect(events.at(-1)).toMatchObject({type:"failed",message:"ChatGPT did not stop the previous response; the new message was not sent"});
  expect(await page.locator("body").getAttribute("data-confirmations")).toBe("1");
  expect(await page.locator("body").getAttribute("data-stops")).toBe("1");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
  expect((await coordinator.health()).activeActionId).toBeNull();
  expect(documentLoads).toBe(2);
},30_000);

test.skipIf(!native)("an unresponsive iframe Stop confirmation waits twenty seconds without reloading",async()=>{
  const driver=await fixture("",true,connector,0,true);
  stopConfirmation={frame:"srcdoc",persists:true};
  await syncFixture();const loads=documentLoads,began=Date.now();
  const coordinator=new SingleWindowCoordinator(driver,build),offline=offlineRequest();
  await executeCoordinatorFixture(coordinator,offline);
  const events=[];for await(const event of coordinator.events(offline.actionId,0))events.push(event);
  expect(events.at(-1)).toMatchObject({type:"failed",message:"ChatGPT did not stop the previous response; the new message was not sent"});
  expect(Date.now()-began).toBeGreaterThanOrEqual(20_000);expect(Date.now()-began).toBeLessThan(28_000);
  expect(await page.locator("body").getAttribute("data-stops")).toBe("1");
  expect(await page.locator("body").getAttribute("data-confirmations")).toBe("1");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
  expect((await coordinator.health()).activeActionId).toBeNull();
  expect(documentLoads).toBe(loads);
},30_000);

test.skipIf(!native)("an already-open captured Stop confirmation gets twenty seconds without reloading",async()=>{
  const driver=await fixture("",true,connector,0,true);
  stopConfirmation={captured:true,preexisting:true,persists:true};
  await syncFixture();const loads=documentLoads,began=Date.now();
  const coordinator=new SingleWindowCoordinator(driver,build),offline=offlineRequest();
  await executeCoordinatorFixture(coordinator,offline);
  const events=[];for await(const event of coordinator.events(offline.actionId,0))events.push(event);
  expect(events.at(-1)).toMatchObject({type:"failed",message:"ChatGPT did not stop the previous response; the new message was not sent"});
  expect(Date.now()-began).toBeGreaterThanOrEqual(20_000);expect(Date.now()-began).toBeLessThan(28_000);
  expect(await page.locator('body').getAttribute('data-stops')).toBe('0');
  expect(await page.locator('body').getAttribute('data-confirmations')).toBe('1');
  expect(await page.locator('body').getAttribute('data-sends')).toBe('0');
  expect(documentLoads).toBe(loads);
},30_000);

test.skipIf(!native)("a cancelled Stop confirmation never inserts or sends a prompt",async()=>{
  const driver=await fixture("",true,connector,0,true);
  stopConfirmation={frame:"nested",persists:true};
  const controller=new AbortController();
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  const running=executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:controller.signal,emit:()=>{}}).then(()=>undefined,error=>error);
  await page.waitForFunction(()=>document.body.dataset.confirmations==="1");
  controller.abort();
  expect(await running).toBeDefined();
  expect(await page.locator("body").getAttribute("data-confirmations")).toBe("1");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
  expect(await composerText(await activeComposer(page))).toBe("");
},10_000);

for(const locale of [
  {code:"el",name:"Greek",medium:"Μεσαίο",extraHigh:"Πολύ υψηλό",message:"Συνέχισε από εδώ"},
  {code:"it",name:"Italian",medium:"Medio",extraHigh:"Molto alto",message:"Continua da qui"},
] as const) {
test.skipIf(!native)(`${locale.name} pairing borrows Medium and restores the saved effort without English labels`,async()=>{
  const driver=await fixture();fixtureLanguage=locale.code;
  const events:Parameters<DriverExecution["emit"]>[0][]=[];
  await executeFixture(driver,{request,renderedPrompt:prompt,signal:new AbortController().signal,emit:event=>events.push(event)});
  expect(await page.locator("body").getAttribute("data-sent-effort")).toBe(locale.medium);
  expect(await page.locator("body").getAttribute("data-effort")).toBe("Pro");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
  expect(events.at(-1)).toMatchObject({type:"submission_complete"});
},15_000);

test.skipIf(!native)(`${locale.name} selection, Stop fallback and confirmation send one ordinary message`,async()=>{
  const driver=await fixture("",true,connector,0,true);fixtureLanguage=locale.code;localizedStopFallback=true;stopConfirmation={delayMs:200};
  const submitted:ExecuteActionRequest={...request,model:"gpt-6-sol",effort:"xhigh",prompt:{kind:"offline_chat",batchId:`${locale.code}-message`,messages:[{id:locale.code,markdown:locale.message}]}};
  const events:Parameters<DriverExecution["emit"]>[0][]=[];
  await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:event=>events.push(event)});
  expect(await page.locator("body").getAttribute("data-sent-effort")).toBe(locale.extraHigh);
  expect(await page.locator("body").getAttribute("data-order")).toBe("stop,confirm,stopped,send,");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
  expect(await page.locator("body").getAttribute("data-sent-text")).toContain(locale.message);
  expect(events).toContainEqual({type:"model_verified",model:"gpt-6-sol",effort:"xhigh"});
  expect(events.at(-1)).toMatchObject({type:"submission_complete"});
},15_000);

test.skipIf(!native)(`${locale.name} unavailable-model fallback preserves the preferred selection and sends once`,async()=>{
  const driver=await fixture("",true,connector,0,false,true,{family:"6",value:3,locked:[4]});fixtureLanguage=locale.code;
  const submitted=offlineRequest("gpt-6-astra","max");
  const events:Parameters<DriverExecution["emit"]>[0][]=[];
  await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:event=>events.push(event)});
  expect(events).toContainEqual({type:"model_verified",model:"gpt-5.6-sol",effort:"max"});
  expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
  expect(submitted.model).toBe("gpt-6-astra");
},15_000);

}

test.skipIf(!native)("offline Stop requires the paired conversation and account before clicking",async()=>{
  for (const mismatch of ["conversation","account"]) {
    const driver=await fixture("",true,connector,0,true);
    const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1},
      ...(mismatch==="conversation"?{expectedUrl:"https://chatgpt.com/c/other"}:{expectedAccountFingerprint:"other-account"})};
    const failure=await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
    expect(failure).toMatchObject({code:mismatch==="conversation"?"conversation_unavailable":"authentication_required"});
    expect(await page.locator("body").getAttribute("data-stops")).toBe("0");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
  }
},10_000);

test.skipIf(!native)("slow Stop completion, with or without confirmation, sends once without retrying or reloading",async()=>{
  for(const confirmation of [false,true]){
    const driver=await fixture("",true,connector,0,true);
    stopFinishDelayMs=15_000;
    if(confirmation)stopConfirmation={frame:"nested",delayMs:200};
    await syncFixture();const loads=documentLoads,began=Date.now();
    const coordinator=new SingleWindowCoordinator(driver,build);
    const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
    await executeCoordinatorFixture(coordinator,offline);
    const events=[];for await(const event of coordinator.events(offline.actionId,0))events.push(event);
    expect(events.at(-1)).toMatchObject({type:'submission_complete'});
    expect(Date.now()-began).toBeLessThan(26_000);
    expect(documentLoads).toBe(loads);
    expect(await page.locator('body').getAttribute('data-stops')).toBe('1');
    expect(await page.locator('body').getAttribute('data-confirmations')).toBe(confirmation?'1':'0');
    expect(await page.locator('body').getAttribute('data-order')).toBe(confirmation?'stop,confirm,stopped,send,':'stop,stopped,send,');
  }
},55_000);

test.skipIf(!native)("a late Stop confirmation gets its own settling window before any retry or reload",async()=>{
  const driver=await fixture("",true,connector,0,true);
  stopConfirmation={frame:"url",delayMs:18_000};stopFinishDelayMs=5_000;
  await syncFixture();const loads=documentLoads;
  const coordinator=new SingleWindowCoordinator(driver,build),offline:ExecuteActionRequest={...request,prompt:{kind:'offline_goal',goalRevision:1}};
  await executeCoordinatorFixture(coordinator,offline);
  const events=[];for await(const event of coordinator.events(offline.actionId,0))events.push(event);
  expect(events.at(-1)).toMatchObject({type:'submission_complete'});
  const times=await page.evaluate(()=>({confirmed:Number(document.body.dataset.confirmedAt),stopped:Number(document.body.dataset.stoppedAt),sent:Number(document.body.dataset.sentAt)}));
  expect(times.stopped-times.confirmed).toBeGreaterThanOrEqual(5_000);
  expect(times.sent-times.confirmed).toBeLessThan(20_000);
  expect(await page.locator('body').getAttribute('data-stops')).toBe('1');
  expect(await page.locator('body').getAttribute('data-confirmations')).toBe('1');
  expect(await page.locator('body').getAttribute('data-order')).toBe('stop,confirm,stopped,send,');
  expect(documentLoads).toBe(loads);
},40_000);

test.skipIf(!native)("an ignored first Stop is retried automatically before sending once",async()=>{
  const driver=await fixture("",true,connector,0,true,2);
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(await page.locator("body").getAttribute("data-order")).toBe("stop,stop,stopped,send,");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
  expect(documentLoads).toBe(1);
},30_000);

test.skipIf(!native)("a stuck Stop is rehydrated and stopped automatically before sending once",async()=>{
  const driver=await fixture("",true,connector,0,true,"reload");
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  const coordinator=new SingleWindowCoordinator(driver,build),began=Date.now();
  await executeCoordinatorFixture(coordinator,offline);
  const events=[];for await(const event of coordinator.events(offline.actionId,0))events.push(event);
  expect(events.at(-1)).toMatchObject({type:'submission_complete'});
  expect(Date.now()-began).toBeGreaterThanOrEqual(40_000);
  expect(documentLoads).toBe(2);
  expect(await page.locator("body").getAttribute("data-order")).toBe("stop,stopped,send,");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
},55_000);

test.skipIf(!native)("Stop recovery clears an existing draft and reloads before sending once",async()=>{
  const driver=await fixture("A draft I am writing",true,connector,0,true,"reload");
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}});
  expect(documentLoads).toBe(2);
  expect(await composerText(await activeComposer(page))).toBe("");
  expect(await page.locator("body").getAttribute("data-sends")).toBe("1");
  expect(await page.locator("body").getAttribute("data-sent-text")).not.toContain("A draft I am writing");
},55_000);

test.skipIf(!native)("Stop recovery revalidates the account and conversation after reload",async()=>{
  for(const mismatch of ["account","conversation"]){
    const driver=await fixture("",true,connector,0,true,false);
    changedAccountAfterReload=mismatch==="account";changedUrlAfterReload=mismatch==="conversation";
    const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
    const failure=await executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:new AbortController().signal,emit:()=>{}}).then(()=>undefined,error=>error);
    expect(failure).toMatchObject({code:mismatch==="account" ? "authentication_required" : "conversation_unavailable"});
    expect(documentLoads).toBe(mismatch==="account"?2:3);
    expect(await page.locator("body").getAttribute("data-stops")).toBe("0");
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
  }
},100_000);

test.skipIf(!native)("a cancelled offline Stop never inserts or sends its prompt",async()=>{
  const driver=await fixture("",true,connector,0,true,false);
  const controller=new AbortController();
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  const run=executeFixture(driver,{request:offline,renderedPrompt:renderActionPrompt(offline.prompt),signal:controller.signal,emit:()=>{}}).then(()=>undefined,error=>error);
  await page.waitForFunction(()=>document.body.dataset.stops==="1");
  controller.abort();
  expect(await run).toBeDefined();
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(0);
  expect(await composerText(await activeComposer(page))).toBe("");
},10_000);

test.skipIf(!native)("an unresponsive Stop times out before Send and releases browser ownership",async()=>{
  const driver=await fixture("",true,connector,0,true,false);
  const coordinator=new SingleWindowCoordinator(driver,build);
  const offline:ExecuteActionRequest={...request,prompt:{kind:"offline_goal",goalRevision:1}};
  const began=Date.now();
  await executeCoordinatorFixture(coordinator,offline);
  const events=[];for await(const event of coordinator.events(offline.actionId,0)) events.push(event);
  expect(Date.now()-began).toBeGreaterThanOrEqual(60_000);expect(Date.now()-began).toBeLessThan(75_000);
  expect(events.at(-1)).toMatchObject({type:"failed",message:"ChatGPT did not stop the previous response; the new message was not sent"});
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(0);
  expect((await coordinator.action(offline.actionId))?.sendAccepted).toBeFalse();
  expect((await coordinator.health()).activeActionId).toBeNull();
},80_000);

test.skipIf(!native)("chat ID survives project, ordinary-route, and slug rewrites during submission",async()=>{
  const project="g-p-0123456789abcdef0123456789abcdef";
  const conversation="12345678-1234-4321-8765-123456789abc";
  const shortUrl=`https://chatgpt.com/g/${project}/c/${conversation}`;
  const longUrl=`https://chatgpt.com/g/${project}-original-project/c/${conversation}`;
  for (const navigationUrl of [shortUrl, `https://chatgpt.com/c/${conversation}`, shortUrl.replace(project,"g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")]) {
  await fixture();
  const rewritingSurface:SingleBrowserSurface={...surface,navigate:async destination=>{
    const loaded=await surface.navigate(destination);
    await loaded.evaluate(({navigationUrl,longUrl})=>{
      history.replaceState({},"",navigationUrl);
      document.querySelector('[contenteditable="true"]')!.addEventListener('input',()=>history.replaceState({},"",longUrl),{once:true});
    },{navigationUrl,longUrl});
    return loaded;
  }};
  const driver=new ChatGptBrowserDriver(rewritingSurface,build);
  const coordinator=new SingleWindowCoordinator(driver,build);
  const projected:ExecuteActionRequest={...request,url:longUrl,expectedUrl:longUrl,prompt:{kind:"offline_goal",goalRevision:1}};
  await executeCoordinatorFixture(coordinator,projected);
  const events=[];for await(const event of coordinator.events(projected.actionId,0))events.push(event);
  expect(events.at(-1)).toMatchObject({type:"submission_complete",canonicalUrl:`https://chatgpt.com/c/${conversation}`});
  expect((await coordinator.action(projected.actionId))?.canonicalUrl).toBe(`https://chatgpt.com/c/${conversation}`);
  expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(1);
  expect((await coordinator.health()).activeActionId).toBeNull();
  }
},25_000);

test.skipIf(!native)("a different chat ID before Send remains rejected",async()=>{
  const expected="https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef-original-project/c/12345678-1234-4321-8765-123456789abc";
  for(const actual of [expected.replace('12345678-1234-4321-8765-123456789abc','other-conversation')]) {
    await fixture();
    const movedSurface:SingleBrowserSurface={...surface,navigate:async destination=>{
      const loaded=await surface.navigate(destination);
      await loaded.evaluate(actual=>document.querySelector('[contenteditable="true"]')!.addEventListener('input',()=>history.replaceState({},"",actual),{once:true}),actual);
      return loaded;
    }};
    const driver=new ChatGptBrowserDriver(movedSurface,build);
    const coordinator=new SingleWindowCoordinator(driver,build);
    const projected:ExecuteActionRequest={...request,url:expected,expectedUrl:expected,prompt:{kind:"offline_goal",goalRevision:1}};
    await executeCoordinatorFixture(coordinator,projected);
    const events=[];for await(const event of coordinator.events(projected.actionId,0))events.push(event);
    expect(events.at(-1)).toMatchObject({type:"failed",code:"conversation_unavailable"});
    expect(await page.locator("body").getAttribute("data-sends")).toBe("0");
    expect((await coordinator.action(projected.actionId))?.sendAccepted).toBeFalse();
  }
},10_000);

test.skipIf(!native)("new chats bind the created URL after sending the initialization prompt",async()=>{
  for(const destination of ["https://chatgpt.com/new","https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/project"]){
    await fixture();
    const creationSurface:SingleBrowserSurface={...surface,navigate:async target=>{
      const loaded=await surface.navigate(target);
      await loaded.evaluate(()=>document.querySelector('form')!.addEventListener('submit',()=>history.replaceState({},"","/c/new-created-chat")));
      return loaded;
    }};
    class CreationDriver extends ChatGptBrowserDriver { override async capabilities(){return {protocolVersion:1 as const,build,accountFingerprint:request.expectedAccountFingerprint,catalogRevision:"fixture",models:[{id:"fixture-model",label:"Fixture model",efforts:[{id:"fixture-effort",label:"Fixture effort",available:true,reason:null}]}]};} }
    const driver=new CreationDriver(creationSurface,build);
    await driver.prepareNewChat(destination==='https://chatgpt.com/new'?null:destination);
    const preparedLoads=documentLoads;
    await page.evaluate(destination=>history.replaceState({},'',destination==='https://chatgpt.com/new'?'/c/local-chatgpt:prepared':'/g/g-p-0123456789abcdef0123456789abcdef-canonical/project'),destination);
    const coordinator=new SingleWindowCoordinator(driver,build);
    const creation:ExecuteActionRequest={...request,url:destination,expectedUrl:destination,prompt:{kind:"create_chat",continuationToken:"A".repeat(43),reference:newReference,setupReference:null}};
    await executeCoordinatorFixture(coordinator,creation);const events=[];for await(const event of coordinator.events(creation.actionId,0))events.push(event);
    expect(events.at(-1)).toMatchObject({type:"submission_complete",canonicalUrl:"https://chatgpt.com/c/new-created-chat"});
    expect(await page.evaluate(()=>document.body.dataset.sentText)).toContain("continue_task");
    expect(await page.evaluate(()=>Number(document.body.dataset.sends))).toBe(1);
    expect(documentLoads).toBe(preparedLoads);
    expect((await coordinator.health()).activeActionId).toBeNull();
  }
},25_000);


test.skipIf(!native)("new chat creation waits for the persistent URL instead of retaining the local draft ID",async()=>{
  await fixture();
  await page.evaluate(()=>history.replaceState({},"","/c/local-chatgpt%3Adraft-fixture"));
  const began=Date.now();
  await page.evaluate(()=>setTimeout(()=>history.replaceState({},"","/c/persistent-fixture"),250));
  const {createdChatUrl}=await import("../src/new-chat");
  expect(await createdChatUrl(page,new AbortController().signal)).toBe("https://chatgpt.com/c/persistent-fixture");
  expect(Date.now()-began).toBeGreaterThanOrEqual(200);
});

test.skipIf(!native)("health polling fetches one session while draft and page state remain live",async()=>{
  const driver=await fixture();
  for(const health of await Promise.all(Array.from({length:20},()=>driver.health())))expect(health.status).toBe('idle');
  for(let i=0;i<10;i++)expect((await driver.health()).authenticated).toBeTrue();
  expect(sessionRequests).toBe(1);
  await page.locator('[contenteditable="true"]').evaluate(element=>element.textContent='A draft I am writing');
  expect((await driver.health()).status).toBe('idle');expect(sessionRequests).toBe(1);
  expect(await composerText(await activeComposer(page))).toBe('A draft I am writing');
  await page.reload();expect((await driver.health()).status).toBe('idle');expect(sessionRequests).toBe(2);
},15_000);

test.skipIf(!native)("a cached healthy identity cannot authorize a send after the account changes",async()=>{
  const driver=await fixture(),coordinator=new SingleWindowCoordinator(driver,build);
  expect((await coordinator.health()).authenticated).toBeTrue();expect(sessionRequests).toBe(1);
  changedAccountAfterReload=true;documentLoads=3;
  expect((await coordinator.health()).authenticated).toBeTrue();expect(sessionRequests).toBe(1);
  await executeCoordinatorFixture(coordinator,request);const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe('authentication_required');expect((await coordinator.action(request.actionId))?.sendAccepted).toBeFalse();
  expect(await page.evaluate(()=>document.body.dataset.sends)).toBe('0');expect(sessionRequests).toBe(2);
},15_000);


test.skipIf(!native)("a created conversation URL is retained when applying the saved model fails after Send",async()=>{
 const driver=await fixture("",true,connector,0,false,true,{family:"5.6",value:2,locked:[]});
 const create:ExecuteActionRequest={...request,url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",model:"gpt-99-sol",effort:"max",prompt:{kind:"create_chat",reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null}};
 const coordinator=new SingleWindowCoordinator(driver,build),events:Parameters<DriverExecution['emit']>[0][]=[];
 await executeCoordinatorFixture(coordinator,create);for await(const event of coordinator.events(create.actionId,0))events.push(event);
 const created=events.findIndex(event=>event.type==='conversation_created');
 expect(created).toBeGreaterThan(events.findIndex(event=>event.type==='send_accepted'));
 expect(events[created]).toMatchObject({type:'conversation_created',canonicalUrl:'https://chatgpt.com/c/new-created-fixture'});
 expect(events.at(-1)?.type).not.toBe('submission_complete');
 expect((await coordinator.action(create.actionId))?.canonicalUrl).toBe('https://chatgpt.com/c/new-created-fixture');
 expect((await coordinator.action(create.actionId))?.sendAccepted).toBeTrue();
 expect(await page.evaluate(()=>document.body.dataset.sends)).toBe('1');
},20_000);


async function lazyJapanesePicker() {
  await page.evaluate(()=>{
    const script=document.createElement('script');script.textContent=`(()=>{
      const view=document.querySelector('[data-model-picker-view]');
      view.querySelector('[data-family="5.5"]')?.remove();
      const rows=[...view.querySelectorAll('[role="menuitemradio"]')];
      rows.forEach(row=>row.remove());view.dataset.modelPickerView='simple';
      const toggle=document.createElement('button');toggle.type='button';toggle.setAttribute('role','menuitem');
      toggle.setAttribute('aria-expanded','false');toggle.textContent='Instant ›';view.append(toggle);
      const close=()=>{rows.forEach(row=>row.remove());toggle.setAttribute('aria-expanded','false');view.dataset.modelPickerView='simple';};
      toggle.onclick=()=>{view.append(...rows);toggle.setAttribute('aria-expanded','true');view.dataset.modelPickerView='advanced';window.renderModel();};
      rows.forEach(row=>row.addEventListener('click',close));
      document.addEventListener('keydown',event=>{if(event.key==='Escape')close();});
      for(const family of ['6','5.6'])Object.assign(window.definitions[family],{labels:['Instant','中','高'],value:2,locked:[]});
      document.querySelector('#effort').removeAttribute('data-testid');window.renderModel();
    })();`;document.body.append(script);
  });
}

for(const creating of [false,true])test.skipIf(!native)(`Japanese lazy picker ${creating?'creates a new chat':'pairs an existing chat'} with one Medium send`,async()=>{
  const driver=await fixture('',true,connector,0,false,true,{family:'6',value:2,locked:[]});
  fixtureLanguage='ja';await syncFixture();await lazyJapanesePicker();
  if(creating)await page.evaluate(()=>history.replaceState({},'','/'));
  const catalog=await driver.capabilities();
  expect(catalog.models.map(model=>model.id)).toEqual(['gpt-6-sol','gpt-5.6-sol']);
  const submitted:ExecuteActionRequest=creating?{...request,url:'https://chatgpt.com/new',expectedUrl:'https://chatgpt.com/new',model:'gpt-6-sol',effort:'high',prompt:{kind:'create_chat',reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null}}:request;
  const coordinator=new SingleWindowCoordinator(driver,build);
  await executeCoordinatorFixture(coordinator,submitted);const events=[];
  for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe('submission_complete');
  expect(events.filter(event=>event.type==='send_accepted')).toHaveLength(1);
  expect(await page.evaluate(()=>[document.body.dataset.sends,document.body.dataset.sentFamily,document.body.dataset.sentEffort,document.body.dataset.family,document.body.dataset.effort])).toEqual(['1','6','中','6','高']);
  await coordinator.close();
},30_000);

const conversationLimitHtml=readFileSync(new URL('./fixtures/conversation-limit-alert.html',import.meta.url),'utf8');
test.skipIf(!native)('re-pairing sends the normal prompt in the retained conversation without another load',async()=>{
 const driver=await fixture(),coordinator=new SingleWindowCoordinator(driver,build);
 const create:ExecuteActionRequest={...request,url:'https://chatgpt.com/new',expectedUrl:'https://chatgpt.com/new',retainPairingWindow:true,agentTicket:'FreshT_1',
   prompt:{kind:'create_chat',reference:newReference,continuationToken:Buffer.alloc(32,2).toString('base64url'),setupReference:null}};
 await executeCoordinatorFixture(coordinator,create);for await(const _ of coordinator.events(create.actionId,0)){}
 const canonicalUrl=page.url(),loads=documentLoads;
 expect(await page.locator('body').getAttribute('data-sent-text')).toContain('FreshT_1');
 expect((await coordinator.health()).activeActionId).toBe(create.actionId);
 await expect(coordinator.accountPlan('pairing-test')).rejects.toThrow('busy');
 await expect(coordinator.execute({...request,actionId:'unrelated',idempotencyKey:'unrelated-key'})).rejects.toThrow('busy');
 const followUp:ExecuteActionRequest={...request,actionId:'paired-normal',idempotencyKey:'paired-normal-key',url:canonicalUrl,expectedUrl:canonicalUrl,
   pairingContinuationOf:create.actionId,agentTicket:'FreshT_2',model:'gpt-6-sol',effort:'high',prompt:{kind:'offline_goal',goalRevision:1}};
 await executeCoordinatorFixture(coordinator,followUp);for await(const _ of coordinator.events(followUp.actionId,0)){}
 expect(page.url()).toBe(canonicalUrl);
 expect(documentLoads).toBe(loads);
 expect(await page.locator('body').getAttribute('data-sends')).toBe('2');
 expect(await page.locator('body').getAttribute('data-sent-text')).toContain('FreshT_2');
 expect((await coordinator.health()).activeActionId).toBeNull();
 await coordinator.close();
},30_000);
test.skipIf(!native)('task continuation resumes a fresh conversation without an old handoff marker',async()=>{
 const driver=await fixture(), events:Array<Parameters<DriverExecution['emit']>[0]>=[];
 const submitted:ExecuteActionRequest={...request,prompt:{kind:'task_continuation'}};
 await executeFixture(driver,{request:submitted,renderedPrompt:renderActionPrompt(submitted.prompt),signal:new AbortController().signal,emit:event=>events.push(event)});
 expect(await page.locator('body').getAttribute('data-sends')).toBe('1');
 expect(events.filter(event=>event.type==='send_accepted')).toHaveLength(1);
 expect(events.at(-1)?.type).toBe('submission_complete');
},20_000);
for(const stage of ["before","preparation","after"] as const)test.skipIf(!native)(`conversation length limit is reported without retrying Send (stage=${stage})`,async()=>{
 const driver=await fixture(),coordinator=new SingleWindowCoordinator(driver,build),submitted=offlineRequest('gpt-6-astra','max');
 await page.evaluate(({html,stage})=>{
  const script=document.createElement('script');script.textContent=`(()=>{const html=${JSON.stringify(html)},stage=${JSON.stringify(stage)};let shown=false;const show=()=>{if(!shown){shown=true;document.body.insertAdjacentHTML('beforeend',html);}};if(stage==='after')document.querySelector('form').addEventListener('submit',()=>setTimeout(show,50));else if(stage==='preparation')document.querySelector('form').addEventListener('input',()=>{if(/goal/.test(document.querySelector('[data-composer-markdown]').textContent))show();});else show();document.addEventListener('click',event=>{if(event.target.closest('[role="alert"] button'))document.body.dataset.errorRetries='1';});})();`;document.body.append(script);
 },{html:conversationLimitHtml,stage});
 await executeCoordinatorFixture(coordinator,submitted);const events=[];for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
 expect(events.at(-1)).toMatchObject({type:'failed',code:'conversation_limit'});
 expect(await page.evaluate(()=>[document.body.dataset.sends,document.body.dataset.errorRetries||'0'])).toEqual([stage==='after'?'1':'0','0']);
 expect((await coordinator.health()).authenticated).toBeTrue();
},20_000);

test.skipIf(!native)('quoted or hidden conversation length warnings do not trigger rollover',async()=>{
 const {conversationLimitVisible}=await import('../src/browser-session');await fixture();
 for(const attributes of ['data-user-message-bubble','data-conversation-role="assistant"','hidden','inert','aria-hidden="true"','style="opacity:0"']){
  await page.evaluate(({html,attributes})=>{document.querySelector('#quoted-limit')?.remove();document.body.insertAdjacentHTML('beforeend',`<div id="quoted-limit" ${attributes}>${html}</div>`);},{html:conversationLimitHtml,attributes});
  expect(await conversationLimitVisible(page)).toBeFalse();
 }
},10_000);

test.skipIf(!native)('a changed account cannot authorize a conversation-length rollover',async()=>{
 const driver=await fixture(),coordinator=new SingleWindowCoordinator(driver,build);await coordinator.health();
 changeAccountAtRequest=2;await page.locator('body').evaluate((body,html)=>body.insertAdjacentHTML('beforeend',html),conversationLimitHtml);
 const submitted=offlineRequest('gpt-6-astra','max');await executeCoordinatorFixture(coordinator,submitted);const events=[];for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
 expect(events.at(-1)?.type).toBe('authentication_required');expect(await page.evaluate(()=>document.body.dataset.sends)).toBe('0');
},10_000);

test.skipIf(!native)('a conversation length alert on a redirected chat cannot replace the requested chat',async()=>{
 await fixture();await page.goto('https://chatgpt.com/c/unrelated-source');
 const driver=new ChatGptBrowserDriver({...surface,navigate:async()=>{await surface.navigate('https://chatgpt.com/c/wrong-target');await page.locator('body').evaluate((body,html)=>body.insertAdjacentHTML('beforeend',html),conversationLimitHtml);return page;}},build);
 const coordinator=new SingleWindowCoordinator(driver,build),submitted=offlineRequest('gpt-6-astra','max');
 await executeCoordinatorFixture(coordinator,submitted);const events=[];for await(const event of coordinator.events(submitted.actionId,0))events.push(event);
 expect(events.at(-1)?.type).toBe('external_interference');expect(await page.evaluate(()=>document.body.dataset.sends)).toBe('0');
},10_000);
