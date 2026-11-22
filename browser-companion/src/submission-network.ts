import type {Page,Response,Request} from "patchright";
import {BrowserCompanionError} from "./errors";
import {existsSync} from "node:fs";
import {join} from "node:path";
import {observeReadOnly} from "./observation";

export function submissionTracingEnabled():boolean {
  return process.env.CODEXIFY_BROWSER_TRACE_SUBMISSION==="1"
    ||Boolean(process.env.CODEXIFY_BROWSER_HOME&&existsSync(join(process.env.CODEXIFY_BROWSER_HOME,"trace-submission")));
}
const tracing=submissionTracingEnabled;

function hasConversationLengthError(payload:unknown):boolean {
  const object=(value:unknown):Record<string,unknown>|undefined=>value!==null&&typeof value==='object'&&!Array.isArray(value)
    ?value as Record<string,unknown>:undefined;
  const body=object(payload);
  const codes=[body,object(body?.detail),object(body?.error)].map(error=>error?.code).filter(code=>code!==undefined&&code!==null);
  // Accept the explicit conversation-level code, never translated prose or
  // per-message/context errors; conflicting envelopes remain unclassified.
  return codes.length>0&&codes.every(code=>code==='conversation_length_exceeds_limit');
}

async function responseHasConversationLengthError(response:Response):Promise<boolean> {
  if(typeof response.body!=='function'||typeof response.headerValue!=='function')return false;
  // Inspect only small JSON rejection bodies. Never consume a successful
  // generation stream, retain message text, or infer a code from its wording.
  const limit=64*1024;
  return observeReadOnly((async()=>{
    const [type,length]=await Promise.all([response.headerValue('content-type'),response.headerValue('content-length')]);
    if(!/^application\/(?:[\w.-]+\+)?json(?:\s*;|$)/i.test(type??''))return false;
    if(length!==null&&(!/^\d+$/.test(length.trim())||Number(length)>limit))return false;
    const body=await response.body();
    return body.byteLength<=limit&&hasConversationLengthError(JSON.parse(body.toString('utf8')));
  })(),undefined,1_500).catch(()=>false);
}

/** Observe the UI's own request; never fetch conversation data or replay a send. */
export function observeSubmissionNetwork(page:Page,options:{recoverOversizedContinuation?:boolean;allowSteering?:boolean}={}) {
  let resolve!:(route:'conversation'|'steering')=>void,reject!:(error:unknown)=>void;
  const accepted=new Promise<'conversation'|'steering'>((yes,no)=>{resolve=yes;reject=no;});
  const started=new Map<Request,'conversation'|'steering'>();
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
    const route=started.get(value.request())!;
    if(route==='conversation'&&status===413&&options.recoverOversizedContinuation) {
      reject(new BrowserCompanionError("ChatGPT rejected this continuation because the conversation request is too large (HTTP 413). Continuing in a new conversation.",
        "conversation_limit",{sendAccepted:false}));return;
    }
    if(status>=200&&status<300){resolve(route);return;}
    const failure=new BrowserCompanionError(`ChatGPT's ${route} submission returned HTTP ${status}`,
      status===429?"rate_limited":"failed",{sendAccepted:true});
    if(route==='steering'||![400,413,422].includes(status)){reject(failure);return;}
    void responseHasConversationLengthError(value).then(limited=>reject(limited
      ?new BrowserCompanionError("ChatGPT rejected this message because the conversation reached its length limit. Continuing in a new conversation.",
        "conversation_limit",{sendAccepted:false})
      :failure),()=>reject(failure));
  };
  const failed=(request:Request)=>{
    if(started.has(request))reject(new BrowserCompanionError("ChatGPT's conversation submission failed before server acceptance was confirmed","ambiguous_send",{sendAccepted:true}));
  };
  const trace=(request:Request)=>{
    if(conversationRequest(request))started.set(request,'conversation');
    if(request.method()!=="POST")return;
    try {const url=new URL(request.url());if(url.origin==="https://chatgpt.com"){
      const path=url.pathname.replace(/[0-9a-f]{8}-[0-9a-f-]{27,}/gi,"<id>");paths.add(path);
      if(tracing())console.error(`[submission-network] POST ${path}`);
      if(/^\/backend-api\/(?:f\/)?steer_turn\/?$/.test(url.pathname)) {
        // Owner instructions may legitimately steer the existing response.
        // The caller must also prove that a new user turn appeared in the UI.
        if(options.allowSteering)started.set(request,'steering');
        else reject(new BrowserCompanionError("ChatGPT routed the message into the previous turn; a fresh turn was not confirmed","ambiguous_send",{sendAccepted:true}));
      }
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
      try {return await Promise.race([accepted,stopped]);}
      catch(error){if(tracing())console.error(`[submission-network] unconfirmed; observed POST paths: ${[...paths].join(", ")||"none"}`);throw error;}
      finally {clearTimeout(timer);signal.removeEventListener("abort",onAbort);}
    },
    close(){page.off("response",response);page.off("requestfailed",failed);page.off("request",trace);},
  };
}
