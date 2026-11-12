import type {BrowserContext, Page} from "patchright";
import {BrowserCompanionError,ConversationCooldownError} from "./errors";
import {AsyncLocalStorage} from "node:async_hooks";
import {readFileSync,lstatSync,writeFileSync,renameSync,unlinkSync,existsSync} from "node:fs";
import {randomUUID} from "node:crypto";
import {join,dirname} from "node:path";
import {setTimeout as delay} from "node:timers/promises";
import {canonicalChatGptConversationUrl} from "./protocol";

const cooldownBypass = new AsyncLocalStorage<boolean>();
export function withConversationCooldownBypass<T>(allowed:boolean, operation:()=>Promise<T>):Promise<T> {
  return cooldownBypass.run(allowed,operation);
}

// One budget for the shared browser, including returns from account settings.
const defaultTimings = {
  // Healthy agents share this tab. Local spacing must not impose the server's
  // rate-limit backoff on every successful switch between their conversations.
  conversationLoadIntervalSeconds:1,
  conversationUnavailableCooldownSeconds:300,
  rateLimitCooldownSeconds:180,
};
let timings = {...defaultTimings};

// Set once at startup from the server's validated settings, before any loads.
export function configureNavigationTimings(raw?:string):void {
  const supplied:unknown=raw===undefined?{}:JSON.parse(raw);
  if(!supplied||typeof supplied!=="object"||Array.isArray(supplied))throw new Error("Invalid browser navigation timings");
  const next={...defaultTimings};
  for(const key of Object.keys(next) as (keyof typeof next)[]){
    const value=(supplied as Record<string,unknown>)[key]??next[key];
    if(typeof value!=="number"||!Number.isInteger(value)||value<1||value>86_400)throw new Error(`Invalid browser navigation timing: ${key}`);
    next[key]=value;
  }
  timings=next;
}
type Budget = {lastLoad:number|null;lastSend:number|null;sendTail:Promise<void>; blockedUntil:number; sendBlockedUntil:number; unavailableUntilByConversation:Map<string,number>; tail:Promise<void>};
const budgets = new WeakMap<BrowserContext,Budget>();
const observedPages = new WeakSet<Page>();
const requestDocuments=new WeakMap<object,string>();
const pageReadLimits=new WeakMap<Page,{url:string;until:number}>();
let rateStatePath:string|undefined,persistedBlockedUntil=0,persistedSendBlockedUntil=0;

export function configureNavigationPersistence(path?:string):void {
  rateStatePath=path;persistedBlockedUntil=0;persistedSendBlockedUntil=0;
  if(!path)return;
  try {
    const metadata=lstatSync(path);
    if(!metadata.isFile()||metadata.isSymbolicLink()||metadata.size>4096)return;
    const saved=JSON.parse(readFileSync(path,"utf8")),value=saved.serverBlockedUntilMs;
    if(Number.isSafeInteger(value)&&value>Date.now())persistedBlockedUntil=value;
    const send=saved.version===2?saved.submissionBlockedUntilMs:value;
    if(Number.isSafeInteger(send)&&send>Date.now())persistedSendBlockedUntil=send;
  }catch{}
}

function persistRateLimit(until:number,sendUntil:number):void {
  if(!rateStatePath)return;
  persistedBlockedUntil=Math.max(persistedBlockedUntil,until);
  persistedSendBlockedUntil=Math.max(persistedSendBlockedUntil,sendUntil);
  const temporary=`${rateStatePath}.${randomUUID()}.tmp`;
  try {
    writeFileSync(temporary,JSON.stringify({version:2,serverBlockedUntilMs:persistedBlockedUntil,submissionBlockedUntilMs:persistedSendBlockedUntil}),{mode:0o600,flag:'wx'});
    renameSync(temporary,rateStatePath);
  }catch{}
  finally{try{unlinkSync(temporary);}catch{}}
}

function recordRateLimit(page:Page,retryAfter:string|null,submission=false,documentUrl=page.url()):void {
  const parsed=retryAfter?.trim()&&/^\d+$/.test(retryAfter.trim())?Number(retryAfter.trim())*1_000:Date.parse(retryAfter??"")-Date.now();
  const delay=Math.max(timings.conversationLoadIntervalSeconds*1_000,Number.isFinite(parsed)&&parsed>=0?parsed:timings.rateLimitCooldownSeconds*1_000);
  const budget=navigationBudget(page.context());
  budget.blockedUntil=Math.max(budget.blockedUntil,Date.now()+delay);
  if(submission)budget.sendBlockedUntil=Math.max(budget.sendBlockedUntil,Date.now()+delay);
  else {
    const previous=pageReadLimits.get(page);
    pageReadLimits.set(page,{url:documentUrl,until:Math.max(previous?.url===documentUrl?previous.until:0,Date.now()+delay)});
  }
  persistRateLimit(budget.blockedUntil,budget.sendBlockedUntil);
}

/** The document is often 200 while ChatGPT's conversation fetch is throttled. */
export function observeConversationRateLimits(page:Page):void {
  if(observedPages.has(page)||typeof page.on!=="function")return;
  observedPages.add(page);
  page.on("request",request=>{if(request.method()==="GET")requestDocuments.set(request,page.url());});
  page.on("response",response=>{
    let url:URL;try{url=new URL(response.url());}catch{return;}
    if(response.status()!==429||url.origin!=="https://chatgpt.com"
      ||!/^\/(?:backend-api|backend-anon)\/(?:f\/)?(?:conversation|steer_turn)(?:\/|$)/.test(url.pathname))return;
    const request=response.request?.(),submission=request?.method()==="POST";
    const documentUrl=request?requestDocuments.get(request)??page.url():page.url();
    recordRateLimit(page,null,submission,documentUrl);
    if(process.env.CODEXIFY_BROWSER_TRACE_SUBMISSION==="1"||rateStatePath&&existsSync(join(dirname(rateStatePath),'trace-submission')))
      console.error(`[conversation-rate-limit] ${submission?'POST':'GET'} ${url.pathname.replace(/[0-9a-f]{8}-[0-9a-f-]{27,}/gi,'<id>')}`);
    void response.headerValue("retry-after").then(value=>{if(value)recordRateLimit(page,value,submission,documentUrl);}).catch(()=>{});
  });
}

export function throwIfConversationRateLimited(page:Page,includeFailedHistory=false):void {
  const budget=budgets.get(page.context());
  const sendUntil=Math.max(budget?.sendBlockedUntil??0,persistedSendBlockedUntil);
  const local=pageReadLimits.get(page);
  const readUntil=includeFailedHistory&&local?.url===page.url()&&isConversationUrl(new URL(page.url()))?local.until:0;
  if(Math.max(sendUntil,readUntil)>Date.now())
    throw new BrowserCompanionError("ChatGPT conversation loading is temporarily rate limited; wait before retrying","rate_limited");
}

const rateLimitedConversations = new WeakMap<Page,{url:string;until:number}>();
const loadObservedPages = new WeakSet<Page>();

function recordLoadRateLimit(page:Page,retryAfter:string|null):void {
  const budget=navigationBudget(page.context());
  const value=retryAfter?.trim();
  const delay=value&&/^\d+$/.test(value)?Number(value)*1_000:Date.parse(value??'')-Date.now();
  budget.blockedUntil=Math.max(budget.blockedUntil,Date.now()+Math.max(timings.conversationLoadIntervalSeconds*1_000,
    Number.isFinite(delay)&&delay>=0?delay:timings.rateLimitCooldownSeconds*1_000));
  try{rateLimitedConversations.set(page,{url:canonicalChatGptConversationUrl(page.url()),until:budget.blockedUntil});}catch{}
}

export function conversationLoadWasRateLimited(page:Page):boolean {
  try{const failure=rateLimitedConversations.get(page);return !!failure&&failure.until>Date.now()&&failure.url===canonicalChatGptConversationUrl(page.url());}catch{return false;}
}

/** ChatGPT can serve the document successfully and rate-limit its transcript API. */
export function observeConversationLoadResponses(page:Page):void {
  if(loadObservedPages.has(page)||typeof page.on!=="function")return;
  loadObservedPages.add(page);
  page.on('response',response=>{
    void (async()=>{
      const request=response.request(),url=new URL(response.url());
      if(request.method()!=='GET'||url.origin!=='https://chatgpt.com')return;
      const match=/^\/backend-api\/(?:f\/)?conversations?\/([^/]+)\/?$/.exec(url.pathname);
      if(!match)return;
      const current=canonicalChatGptConversationUrl(page.url());
      if(current!==canonicalChatGptConversationUrl('https://chatgpt.com/c/'+match[1]))return;
      if(response.status()===429){
        const retryAfter=await response.headerValue('retry-after');
        if(canonicalChatGptConversationUrl(page.url())===current)recordLoadRateLimit(page,retryAfter);
      }else if(response.status()>=200&&response.status()<300)rateLimitedConversations.delete(page);
    })().catch(()=>{});
  });
}

function navigationBudget(context:BrowserContext):Budget {
  let budget=budgets.get(context);
  if(!budget){budget={lastLoad:null,lastSend:null,sendTail:Promise.resolve(),blockedUntil:persistedBlockedUntil,sendBlockedUntil:persistedSendBlockedUntil,unavailableUntilByConversation:new Map(),tail:Promise.resolve()};budgets.set(context,budget);}
  return budget;
}

/** Serialize the irreversible Send boundary across pages, not navigation or
 * generation. Final draft/account/model checks remain inside this lease. */
export async function acquireSubmissionSlot(page:Page,signal?:AbortSignal):Promise<{markSent():void;release():void}> {
  signal?.throwIfAborted();
  const budget=navigationBudget(page.context()),previous=budget.sendTail;
  let unlock!:()=>void;
  budget.sendTail=new Promise<void>(resolve=>{unlock=resolve;});
  let acquired=false,released=false;
  const release=()=>{if(!released){released=true;unlock();}};
  let abort:(()=>void)|undefined;
  try {
    await Promise.race([previous,new Promise<never>((_resolve,reject)=>{
      abort=()=>reject(signal?.reason??new DOMException("Browser action aborted","AbortError"));
      if(signal?.aborted)abort();else signal?.addEventListener("abort",abort,{once:true});
    })]);
    acquired=true;signal?.throwIfAborted();
    if(budget.lastSend!==null)await delay(Math.max(0,1000-(Date.now()-budget.lastSend)),undefined,{signal});
    signal?.throwIfAborted();throwIfConversationRateLimited(page);
    return {release,markSent:()=>{signal?.throwIfAborted();throwIfConversationRateLimited(page);budget.lastSend=Date.now();}};
  } catch(error) {
    if(acquired)release();else void previous.then(release);
    throw error;
  } finally {if(abort)signal?.removeEventListener("abort",abort);}
}

function isConversationUrl(url:URL):boolean {
  return url.origin === "https://chatgpt.com" && /^\/(?:g\/[^/]+\/)?c\/[^/]+\/?$/.test(url.pathname);
}

function conversationKey(url:URL):string|undefined {
  if(url.origin!=="https://chatgpt.com")return undefined;
  const match=/^\/(?:g\/[^/]+\/)?c\/([^/]+)\/?$/.exec(url.pathname);
  return match?`https://chatgpt.com/c/${match[1]}`:undefined;
}

export function conversationLoadAvailableAt(page:Page,url:string):number|null {
  const target=new URL(url);
  if(!isConversationUrl(target))return null;
  try {if(canonicalChatGptConversationUrl(page.url())===canonicalChatGptConversationUrl(url))return null;} catch {}
  const budget=budgets.get(page.context());
  if(!budget)return null;
  const at=Math.max(budget.unavailableUntilByConversation.get(conversationKey(target)??"")??0,budget.blockedUntil,budget.lastLoad===null?0:budget.lastLoad+timings.conversationLoadIntervalSeconds*1_000);
  return at>Date.now()?at:null;
}

export function recordConversationUnavailable(page:Page):void {
  const key=conversationKey(new URL(page.url()));if(!key)return;
  const budget=navigationBudget(page.context());
  // Repeated health checks of the same failure must not postpone recovery.
  if(!budget.unavailableUntilByConversation.has(key))budget.unavailableUntilByConversation.set(key,Date.now()+timings.conversationUnavailableCooldownSeconds*1_000);
}

// Called only after verifying the account and a usable conversation composer.
export function recordConversationAvailable(page:Page):void {
  const key=conversationKey(new URL(page.url()));
  if(key){
    const budget=budgets.get(page.context());
    budget?.unavailableUntilByConversation.delete(key);
    pageReadLimits.delete(page);
    rateLimitedConversations.delete(page);
  }
}

export function isChatGptPage(value:string):boolean {
  try {return new URL(value).origin === "https://chatgpt.com";} catch {return false;}
}

export async function navigatePage(page:Page,url:string,options:{signal?:AbortSignal;timeout?:number;reload?:boolean}={}):Promise<void> {
  observeConversationRateLimits(page);
  options.signal?.throwIfAborted();
  const target = new URL(url);
  if(target.protocol !== "https:")throw new Error("Browser window navigation requires HTTPS");
  if(!options.reload && page.url() === target.href)return;
  const budget = navigationBudget(page.context());
  const previous = budget.tail;
  let release!:()=>void;
  budget.tail = new Promise<void>(resolve=>{release=resolve;});
  let released=false;
  const releaseReservation=()=>{if(!released){released=true;release();}};
  try {
    await previous;
    options.signal?.throwIfAborted();
    if(!options.reload && page.url() === target.href)return;
    const sourceUrl=page.url();
    const key=conversationKey(target),conversation=key!==undefined;
    budget.blockedUntil=Math.max(budget.blockedUntil,persistedBlockedUntil);
    // Manual retries may skip local spacing, never a server rate-limit deadline.
    if(conversation&&budget.blockedUntil>Date.now())throw new ConversationCooldownError(budget.blockedUntil);
    if(conversation&&!cooldownBypass.getStore()){
      const availableAt=Math.max(budget.unavailableUntilByConversation.get(key!)??0,budget.blockedUntil,budget.lastLoad===null?0:budget.lastLoad+timings.conversationLoadIntervalSeconds*1_000);
      if(availableAt>Date.now())throw new ConversationCooldownError(Math.min(availableAt,Number.MAX_SAFE_INTEGER));
      options.signal?.throwIfAborted();
    }
    if(page.url()!==sourceUrl)throw new BrowserCompanionError("ChatGPT changed pages while waiting to navigate", "external_interference");
    if(options.reload&&page.url()!==target.href)throw new BrowserCompanionError("ChatGPT changed pages before recovery reload", "external_interference");
    if(conversation){
      budget.lastLoad=Date.now();
      // Reserve the next slot before loading so even a timeout or another
      // queued retry cannot start a second probe of this failed conversation.
      // Other chats retain only the ordinary spacing and server429 deadlines.
      if(budget.unavailableUntilByConversation.has(key!))budget.unavailableUntilByConversation.set(key!,Date.now()+timings.conversationUnavailableCooldownSeconds*1_000);
    }
    // The budget protects start spacing and real server deadlines. A slow
    // document on one registered page must not retain every other page's gate.
    releaseReservation();
    // Finish a failed load before the coordinator's 30s stall deadline. Native
    // navigation does not reliably cancel on an AbortSignal on every platform.
    const settings={waitUntil:"domcontentloaded" as const,timeout:options.timeout??15_000,signal:options.signal};
    const response=options.reload?await page.reload(settings):await page.goto(target.href,settings);
    if(response?.status()===429){
      // A proxy or browser transport can fail to expose response headers.
      // Preserve the 429 diagnosis and the normal cooldown in that case.
      const retryAfter=await response.headerValue("retry-after").catch(()=>null);
      if(conversation){recordRateLimit(page,retryAfter);recordLoadRateLimit(page,retryAfter);}
      throw new BrowserCompanionError("ChatGPT conversation loading is temporarily rate limited; wait before retrying", conversation?"conversation_load_rate_limited":"rate_limited");
    }
  }finally{releaseReservation();}
}
