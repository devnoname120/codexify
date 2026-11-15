import type {Page} from "patchright";
import {canonicalChatGptConversationUrl} from "./protocol";
import {readChatGptAccount} from "./account";
import {readTurnDomState} from "./turn-binding";
import {chatGptAssistantTurnSelector,CHATGPT_USER_TURN_SELECTOR} from "./session";
import {responseDomSnapshot,type ChatGptResponseDomCache} from "./response-observer";
import {observeReadOnly} from "./observation";

export function handoffReportReady(input:{answer:string;responsePresent:boolean;generationRunning:boolean;completionActionVisible:boolean}, id:string):boolean {
  return /^renew-[a-f0-9]{64}$/.test(id) && input.responsePresent && !input.generationRunning
    && input.completionActionVisible && input.answer.trimStart().startsWith(`Turn handoff ${id}\n`)
    && input.answer.trim().length > `Turn handoff ${id}`.length;
}

// Read-only, same-account, same-conversation proof. Never interpret companion idle,
// missing Stop controls alone, old answers or a streaming report as completion.
export async function completedHandoff(page:Page, url:string, account:string, id:string, signal?:AbortSignal):Promise<boolean> {
  return createCompletedHandoffVerifier(page,url,account,id,signal)();
}

/** Action-local proof only. Rechecks still require fresh identity, no running
 * turn, the same final response/controls and byte-identical report. Once those
 * are unchanged, repeating the initial two-frame delay proves nothing new. */
export function createCompletedHandoffVerifier(page:Page,url:string,account:string,id:string,signal?:AbortSignal):()=>Promise<boolean> {
  let verified:{identity:string;answer:string}|undefined;
  const cache:ChatGptResponseDomCache={};
  return async()=>{
  let previous:string|undefined;
  let identity:string|undefined;
  const attempts=verified?1:2;
  for(let attempt=0;attempt<attempts;attempt++) {
    signal?.throwIfAborted();
    if(canonicalChatGptConversationUrl(page.url())!==canonicalChatGptConversationUrl(url)
      ||(await readChatGptAccount(page,signal)).fingerprint!==account)return false;
    const state=await readTurnDomState(page,undefined,signal);
    const last=state.responseIdentities.at(-1);
    if(!last || state.generationRunning)return false;
    const response=page.locator(chatGptAssistantTurnSelector(last));
    // A submitted Continue with no answer yet must not reuse the old report.
    // Inspect DOM order directly: legacy pages need not expose turn containers.
    if(!await observeReadOnly(response.evaluate((element,userSelector)=>
      ![...document.querySelectorAll(userSelector)].some(user=>
        Boolean(element.compareDocumentPosition(user)&Node.DOCUMENT_POSITION_FOLLOWING)),
      CHATGPT_USER_TURN_SELECTOR),signal))return false;
    const snapshot=await responseDomSnapshot(response,cache,signal);
    if(!handoffReportReady({answer:snapshot.visibleText,...snapshot,generationRunning:state.generationRunning},id))return false;
    if(verified)return last===verified.identity&&snapshot.visibleText===verified.answer;
    if(attempt===1){
      if(last!==identity||snapshot.visibleText!==previous)return false;
      verified={identity:last,answer:snapshot.visibleText};return true;
    }
    identity=last;previous=snapshot.visibleText;
    await new Promise<void>((resolve,reject)=>{
      const abort=()=>{clearTimeout(timer);reject(signal?.reason??new Error("Cancelled"));};
      const timer=setTimeout(()=>{signal?.removeEventListener("abort",abort);resolve();},750);
      signal?.addEventListener("abort",abort,{once:true});
    });
  }
  return false;
  };
}
