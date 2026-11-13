import type {Page,Locator} from "patchright";
import {CHATGPT_STOP_BUTTON_SELECTOR,CHATGPT_COMPOSER_SELECTOR} from "./session";
import {uiLabelPattern} from "./ui-localization";
import {existsSync} from "node:fs";
import {join} from "node:path";
export const END_TURN_CONTROL_NAME=/^End(?: this| current)? turn$/iu;

/** Work controls may expose an accessible name without a test ID or aria-label. */
export function stopControls(page:Page):Locator {
  return page.locator(CHATGPT_STOP_BUTTON_SELECTOR)
    .or(page.getByRole('button',{name:uiLabelPattern('stopControl')}))
    .or(page.getByRole('button',{name:END_TURN_CONTROL_NAME}))
    .filter({visible:true});
}

/** Private, opt-in control metadata only: never the composer or transcript. */
export async function traceStopControls(page:Page,phase:string):Promise<void> {
  if(process.env.CODEXIFY_BROWSER_TRACE_SUBMISSION!=="1"
    && !(process.env.CODEXIFY_BROWSER_HOME&&existsSync(join(process.env.CODEXIFY_BROWSER_HOME,'trace-submission'))))return;
  try {
    const frames=[];
    for(const frame of page.frames()) {
      let origin:string;try{origin=new URL(frame.url()).origin;}catch{continue;}
      if(frame!==page.mainFrame()&&origin!=="https://chatgpt.com"&&frame.url()!=="about:blank"&&frame.url()!=="about:srcdoc")continue;
      if(frame!==page.mainFrame()&&!await frame.evaluate(()=>{try{return !!window.parent.document;}catch{return false;}}).catch(()=>false))continue;
      const controls=await frame.locator('button,[role="button"]').evaluateAll((elements,composerSelector)=>elements.map(element=>{
      const clean=(value:string|null)=>value?.replace(/https?:\/\/\S+|[\w.+-]+@[\w.-]+/g,'<redacted>').replace(/\d{8,}/g,'<id>').replace(/\s+/g,' ').trim().slice(0,80)??null;
      const name=clean(element.getAttribute('aria-label')||element.getAttribute('title')||element.textContent);
      const testId=clean(element.getAttribute('data-testid'));
      const form=element.closest('form');
      const visible=Boolean(element.getClientRects().length)&&getComputedStyle(element).visibility!=="hidden";
      const ancestors=Array.from((function*(node:Element|null){for(let i=0;node&&i<5;i++,node=node.parentElement)yield node;})(element.parentElement)).map(node=>({tag:node.tagName,id:clean(node.id),testId:clean(node.getAttribute('data-testid')),display:getComputedStyle(node).display}));
      return {tag:element.tagName,name,testId,visible,ancestors,inComposer:Boolean(form?.querySelector(composerSelector)),disabled:(element as HTMLButtonElement).disabled,
        icon:clean(element.querySelector('use')?.getAttribute('href')??null)};
    }).filter(value=>(value.visible&&value.inComposer)||/stop|end|turn|cancel|pause|resume|interrupt|finish|заверш|останов|прерв/iu.test(`${value.name??''} ${value.testId??''} ${value.icon??''}`)).slice(-24),CHATGPT_COMPOSER_SELECTOR);
      frames.push({main:frame===page.mainFrame(),origin,controls});
    }
    console.error(`[turn-controls] ${phase} ${JSON.stringify(frames)}`);
  }catch{console.error(`[turn-controls] ${phase} unavailable`);}
}
