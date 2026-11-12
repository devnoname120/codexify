import type {Page,Response,Request} from "patchright";
import {BrowserCompanionError} from "./errors";
import {existsSync} from "node:fs";
import {join} from "node:path";

export function submissionTracingEnabled():boolean {
  return process.env.CODEXIFY_BROWSER_TRACE_SUBMISSION==="1"
    ||Boolean(process.env.CODEXIFY_BROWSER_HOME&&existsSync(join(process.env.CODEXIFY_BROWSER_HOME,"trace-submission")));
}
const tracing=submissionTracingEnabled;

/** Observe the UI's own request; never fetch conversation data or replay a send. */
export function observeSubmissionNetwork(page:Page) {
  let resolve!:()=>void,reject!:(error:unknown)=>void;
  const accepted=new Promise<void>((yes,no)=>{resolve=yes;reject=no;});
  const started=new Set<Request>();
  const paths=new Set<string>();
  void accepted.catch(()=>{});
  const conversationRequest=(request:Request):boolean=>{
    if(request.method()!=="POST")return false;
    let url:URL;try{url=new URL(request.url());}catch{return false;}
    return url.origin==="https://chatgpt.com"&&/^\/(?:backend-api|backend-anon)\/(?:f\/)?conversation\/?$/.test(url.pathname);
  };
  const response=(value:Response)=>{
    if(!started.has(value.request()))return;
    const status=value.status();
    if(tracing())console.error(`[submission-network] conversation response HTTP ${status}`);
    if(status>=200&&status<300)resolve();
    else reject(new BrowserCompanionError(`ChatGPT's conversation submission returned HTTP ${status}`,
      status===429?"rate_limited":"failed",{sendAccepted:true}));
  };
  const failed=(request:Request)=>{
    if(started.has(request))reject(new BrowserCompanionError("ChatGPT's conversation submission failed before server acceptance was confirmed","ambiguous_send",{sendAccepted:true}));
  };
  const trace=(request:Request)=>{
    if(conversationRequest(request))started.add(request);
    if(request.method()!=="POST")return;
    try {const url=new URL(request.url());if(url.origin==="https://chatgpt.com"){
      const path=url.pathname.replace(/[0-9a-f]{8}-[0-9a-f-]{27,}/gi,"<id>");paths.add(path);
      if(tracing())console.error(`[submission-network] POST ${path}`);
      if(/^\/backend-api\/(?:f\/)?steer_turn\/?$/.test(url.pathname))
        reject(new BrowserCompanionError("ChatGPT routed the message into the previous turn; a fresh turn was not confirmed","ambiguous_send",{sendAccepted:true}));
    }
    }catch{}
  };
  page.on("response",response);page.on("requestfailed",failed);page.on("request",trace);
  return {
    async wait(signal:AbortSignal,timeoutMs=25_000) {
      let timer:ReturnType<typeof setTimeout>|undefined;
      let onAbort!:()=>void;
      const stopped=new Promise<never>((_yes,no)=>{
        onAbort=()=>no(signal.reason??new DOMException("Submission confirmation cancelled","AbortError"));
        if(signal.aborted)onAbort();else signal.addEventListener("abort",onAbort,{once:true});
        timer=setTimeout(()=>no(new BrowserCompanionError("ChatGPT displayed the message but server submission acceptance was not confirmed","ambiguous_send",{sendAccepted:true})),timeoutMs);
      });
      try {await Promise.race([accepted,stopped]);}
      catch(error){if(tracing())console.error(`[submission-network] unconfirmed; observed POST paths: ${[...paths].join(", ")||"none"}`);throw error;}
      finally {clearTimeout(timer);signal.removeEventListener("abort",onAbort);}
    },
    close(){page.off("response",response);page.off("requestfailed",failed);page.off("request",trace);},
  };
}
