import type {Page} from "patchright";
import {observeReadOnly} from "./observation";
import {CHATGPT_COMPOSER_SELECTOR} from "./session";
import {stopControls} from "./stop-controls";

/** Private diagnostics: counts/styles/timings only; never prompt or answer text. */
export async function inspectPresentation(page:Page,signal?:AbortSignal,screenshot=false) {
  const result=await observeReadOnly(page.evaluate(composerSelector=>{
    const badge=document.getElementById("codexify-fast-indicator");
    const style=document.getElementById("codexify-minimal-chatgpt");
    const turns=[...document.querySelectorAll<HTMLElement>('[data-turn-key], [data-turn-id-container], [data-testid^="conversation-turn-"]')]
      .filter(node=>!node.parentElement?.closest('[data-turn-key], [data-turn-id-container], [data-testid^="conversation-turn-"]'));
    const nav=performance.getEntriesByType("navigation")[0] as PerformanceNavigationTiming|undefined;
    const visible=(node:Element)=>{const css=getComputedStyle(node);return css.display!=="none"&&css.visibility!=="hidden";};
    return {
      chatGptOrigin:location.origin==="https://chatgpt.com",
      // Numeric diagnostics only. Compare heap growth with DOM growth without
      // retaining page contents or taking a heap snapshot of private messages.
      rendererHeap:(()=>{
        const memory=(performance as Performance & {memory?:{usedJSHeapSize:number;totalJSHeapSize:number;jsHeapSizeLimit:number}}).memory;
        return memory?{usedBytes:memory.usedJSHeapSize,totalBytes:memory.totalJSHeapSize,limitBytes:memory.jsHeapSizeLimit}:null;
      })(),
      mode:document.documentElement.dataset.codexifyUi??null,
      simplicity:document.documentElement.dataset.codexifySimplicity??"off",
      stylesheetPresent:Boolean(style),
      styleRevision:document.documentElement.dataset.codexifyStyleRevision??null,
      ownedAdoptedStylesheets:document.adoptedStyleSheets.filter(sheet=>{
        try{return [...sheet.cssRules].some(rule=>'selectorText' in rule&&rule.selectorText==='#codexify-fast-indicator');}catch{return false;}
      }).length,
      sidebar:(()=>{
        const node=document.getElementById('app-shell-sidebar');
        return node?{width:getComputedStyle(node).width,parentWidth:node.parentElement?getComputedStyle(node.parentElement).width:null}:null;
      })(),
      badgeVisible:Boolean(badge&&visible(badge)),
      nodeCount:document.getElementsByTagName("*").length,
      turnCount:turns.length,
      deferredHistoryTurns:turns.filter(node=>getComputedStyle(node).contentVisibility==="auto").length,
      visibleHistoryNavigation:[...document.querySelectorAll('#stage-slideover-sidebar,[data-testid="sidebar"],[data-testid="history-sidebar"],nav[aria-label="Chat history"]')].filter(visible).length,
      composerCount:document.querySelectorAll(composerSelector).length,
      approvalStructure:[...document.querySelectorAll('[data-codex-approval-surface="true"], [data-testid="tool-approval-card"]')].slice(-4).map(card=>{
        const alert=card.querySelector('[role="alert"]'),form=card.querySelector('form');
        const turn=card.closest('[data-turn-key]')??card.closest('[data-turn-id-container],article[data-testid]');
        return {visible:card.getClientRects().length>0,alerts:card.querySelectorAll('[role="alert"]').length,
          forms:card.querySelectorAll('form').length,heading:alert?.querySelector('.text-secondary > span.wrap-anywhere')?.textContent?.trim().slice(0,128)??null,
          groupedTurn:Boolean(card.closest('[data-turn-key]')),legacyTurn:Boolean(card.closest('[data-turn-id-container]')),
          buttons:[...(form?.querySelectorAll('button')??[])].slice(0,5).map(button=>({text:button.textContent?.trim().slice(0,80),
            type:button.getAttribute('type'),popup:button.getAttribute('aria-haspopup'),classes:button.className.slice(0,700)})),
          mentions:[...(turn?.querySelectorAll('[data-prompt-link-href^="app://"], [app-mention-path]')??[])].slice(0,8).map(node=>({
            app:node.getAttribute('data-prompt-link-href'),path:node.getAttribute('app-mention-path'),text:node.textContent?.trim().slice(0,128)}))};
      }),
      // Structural diagnostics only: locate newer composer/effort controls
      // without reading drafts, messages, account names or menu text.
      composerStructure:(()=>{
        const editor=document.querySelector(composerSelector);
        const form=editor?.closest('form');
        const ancestors=[];
        for(let node=editor?.parentElement;node&&ancestors.length<5;node=node.parentElement)
          ancestors.push({tag:node.tagName,classes:node.className.slice(0,220),form:node.tagName==='FORM'});
        return {editable:editor?.getAttribute('contenteditable')??null,hasForm:Boolean(form),ancestors,
          controls:[...document.querySelectorAll('[data-codex-intelligence-trigger], [data-composer-navigation-target], button[aria-haspopup="menu"]')]
            .filter(node=>!node.closest('#app-shell-sidebar,nav,aside')).slice(0,24).map(node=>({
              tag:node.tagName,testId:node.getAttribute('data-testid'),tone:node.getAttribute('data-tone'),
              target:node.getAttribute('data-composer-navigation-target'),intelligence:node.getAttribute('data-codex-intelligence-trigger'),
              sameForm:Boolean(form&&node.closest('form')===form),visible:node.getClientRects().length>0&&getComputedStyle(node).visibility!=='hidden',
              classes:node.getAttribute('class')?.slice(0,200)??''})),};
      })(),
      shellNodes:(()=>{
        const nodes:Array<{tag:string;id:string;classes:string;role:string|null;testId:string|null;depth:number}>=[];
        const visit=(element:Element,depth:number)=>{
          if(nodes.length>=100||depth>7)return;
          nodes.push({tag:element.tagName,id:element.id,classes:element.getAttribute('class')?.slice(0,300)??'',role:element.getAttribute('role'),testId:element.getAttribute('data-testid'),depth});
          if(element.matches('main,form,[data-turn-key],[data-turn-id-container]'))return;
          for(const child of [...element.children].filter(child=>!child.matches('script,style,link')).slice(0,12))visit(child,depth+1);
        };
        visit(document.body,0);return nodes;
      })(),
      viewport:{width:innerWidth,height:innerHeight},
      navigation:nav?{responseMs:nav.responseEnd,interactiveMs:nav.domInteractive,domContentLoadedMs:nav.domContentLoadedEventEnd}:null,
    };
  },CHATGPT_COMPOSER_SELECTOR),signal,3000);
  let session:Awaited<ReturnType<ReturnType<Page["context"]>["newCDPSession"]>>|undefined;
  let retainedDom:{documents:number;nodes:number;jsEventListeners:number}|null=null;
  try {
    const connecting=page.context().newCDPSession(page);
    try {session=await observeReadOnly(connecting,signal,1000);}
    catch(error){void connecting.then(late=>late.detach(),()=>{}).catch(()=>{});throw error;}
    retainedDom=await observeReadOnly(session.send("Memory.getDOMCounters"),signal,1000);
  } catch { /* A missing diagnostic must not block normal browser work. */ }
  finally {if(session)await observeReadOnly(session.detach(),undefined,1000).catch(()=>{});}
  signal?.throwIfAborted();
  // An accepted Send releases the browser action while its response can keep
  // running. Idle action ownership alone is not permission to restart Chrome.
  const generationRunning=await observeReadOnly(stopControls(page).count(),signal,1000)>0;
  const diagnostics={...result,retainedDom,generationRunning,observedActionPages:1,companionMemory:process.memoryUsage()};
  if(!screenshot)return diagnostics;
  // Existing viewport only; do not scroll, focus, hide the caret or disable
  // animations while another human may be looking at the managed browser.
  const png=await observeReadOnly(page.screenshot({type:"png",fullPage:false,caret:"initial",animations:"allow",timeout:2000}),signal,2500);
  if(png.length>4*1024*1024)throw new Error("Presentation screenshot exceeded its private diagnostic bound");
  return {...diagnostics,screenshotPngBase64:png.toString("base64")};
}
