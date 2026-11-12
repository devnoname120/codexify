import {afterEach,expect,setSystemTime,test} from "bun:test";
import type {BrowserContext,Page} from "patchright";
import {mkdtempSync,unlinkSync,rmdirSync} from "node:fs";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {configureNavigationTimings,configureNavigationPersistence,navigatePage,recordConversationAvailable,recordConversationUnavailable,withConversationCooldownBypass,conversationLoadAvailableAt,observeConversationRateLimits,throwIfConversationRateLimited} from "../src/navigation";

class NavigationPage {
  current="https://chatgpt.com/";
  loads:string[]=[];
  responseCode=200;
  retryAfter:string|null=null;
  headerFailure=false;
  constructor(readonly owner={} as BrowserContext){}
  context=()=>this.owner;
  url=()=>this.current;
  async goto(url:string){this.current=url;this.loads.push(url);return {status:()=>this.responseCode,headerValue:async()=>{if(this.headerFailure)throw new Error("Headers unavailable");return this.retryAfter;}};}
  reload=()=>this.goto(this.current);
  asPage=()=>this as unknown as Page;
}
afterEach(()=>{setSystemTime();configureNavigationTimings();configureNavigationPersistence();});

test("an unsettled document load cannot hold another chat's navigation slot",async()=>{
  const firstPage=new NavigationPage(),secondPage=new NavigationPage(firstPage.owner);
  const now=Date.now();setSystemTime(now);
  let started!:()=>void,release!:()=>void;
  const entered=new Promise<void>(resolve=>{started=resolve;});
  const held=new Promise<void>(resolve=>{release=resolve;});
  const original=firstPage.goto.bind(firstPage);
  firstPage.goto=async url=>{const response=await original(url);started();await held;return response;};
  const first=navigatePage(firstPage.asPage(),'https://chatgpt.com/c/stalled');
  await entered;
  try {
    // Ordinary start spacing still applies while the first load is pending.
    await expect(navigatePage(secondPage.asPage(),'https://chatgpt.com/c/healthy'))
      .rejects.toMatchObject({code:'conversation_cooldown'});
    setSystemTime(now+1_000);
    const result=await Promise.race([
      navigatePage(secondPage.asPage(),'https://chatgpt.com/c/healthy').then(()=>true),
      Bun.sleep(100).then(()=>false),
    ]);
    expect(result).toBe(true);
    expect(secondPage.loads).toEqual(['https://chatgpt.com/c/healthy']);
  } finally {release();await first;}
});

test("passive availability reports the real shared deadline without loading or delaying current-page reuse",async()=>{
 // The healthy interval default is one second; verify a long interval explicitly.
 configureNavigationTimings('{"conversationLoadIntervalSeconds":180}');
 const page=new NavigationPage(),now=Date.now();setSystemTime(now);
 expect(conversationLoadAvailableAt(page.asPage(),'https://chatgpt.com/c/first')).toBeNull();
 await navigatePage(page.asPage(),'https://chatgpt.com/c/first');
 expect(conversationLoadAvailableAt(page.asPage(),page.current)).toBeNull();
 expect(conversationLoadAvailableAt(page.asPage(),'https://chatgpt.com/c/second')).toBe(now+180000);
 setSystemTime(now+30000);expect(conversationLoadAvailableAt(page.asPage(),'https://chatgpt.com/c/second')).toBe(now+180000);
 expect(page.loads).toEqual(['https://chatgpt.com/c/first']);
 setSystemTime(now+180000);expect(conversationLoadAvailableAt(page.asPage(),'https://chatgpt.com/c/second')).toBeNull();
});

test("manual Retry bypasses local cooldowns for its action while automatic loads remain throttled",async()=>{
  const page=new NavigationPage(),now=Date.now();setSystemTime(now);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/first');
  await withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),'https://chatgpt.com/c/manual'));
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/automatic')).rejects.toMatchObject({code:'conversation_cooldown'});
  recordConversationUnavailable(page.asPage());
  await withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),page.current,{reload:true}));
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/automatic')).rejects.toMatchObject({code:'conversation_cooldown'});
  page.responseCode=429;page.retryAfter='900';
  await expect(withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),page.current,{reload:true}))).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  recordConversationAvailable(page.asPage());page.responseCode=200;
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/automatic')).rejects.toMatchObject({code:'conversation_cooldown'});
  await expect(withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),'https://chatgpt.com/c/manual-again'))).rejects.toMatchObject({code:'conversation_cooldown'});
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/automatic')).rejects.toMatchObject({code:'conversation_cooldown'});
  await expect(withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),'https://chatgpt.com/c/cancelled',{signal:AbortSignal.abort()}))).rejects.toThrow();
  expect(page.loads).toHaveLength(4);
});

test("a background conversation API 429 is shared, is not a broken chat, and survives manual Retry",async()=>{
 const page=new NavigationPage(),other=new NavigationPage(page.owner),now=Date.now();setSystemTime(now);
 page.current='https://chatgpt.com/c/rate-limited-history';
 let observe!: (response:any)=>void;
 (page as any).on=(name:string,listener:any)=>{if(name==='response')observe=listener;else expect(name).toBe('request');};
 observeConversationRateLimits(page.asPage());
 observe({url:()=>"https://chatgpt.com/backend-api/conversation/fixture",status:()=>429,headerValue:async()=>"600"});
 await Promise.resolve();
 expect(()=>throwIfConversationRateLimited(page.asPage(),true)).toThrow('rate limited');
 expect(()=>throwIfConversationRateLimited(other.asPage(),true)).not.toThrow();
 await expect(withConversationCooldownBypass(true,()=>navigatePage(other.asPage(),'https://chatgpt.com/c/other'))).rejects.toMatchObject({code:'conversation_cooldown'});
 setSystemTime(now+599_999);expect(()=>throwIfConversationRateLimited(page.asPage(),true)).toThrow();
 setSystemTime(now+600_001);expect(()=>throwIfConversationRateLimited(page.asPage())).not.toThrow();
 await navigatePage(other.asPage(),'https://chatgpt.com/c/other');expect(other.loads).toHaveLength(1);
 observe({url:()=>"https://example.com/backend-api/conversation/fixture",status:()=>429,headerValue:async()=>"600"});
 expect(()=>throwIfConversationRateLimited(page.asPage())).not.toThrow();
});

test("history throttling permits fresh setup, while a submission Retry-After still blocks new sends",async()=>{
 const page=new NavigationPage();page.current='https://chatgpt.com/c/history';
 let response!: (value:any)=>void;(page as any).on=(_name:string,listener:any)=>{response=listener;};
 observeConversationRateLimits(page.asPage());
 const limited=(method:string)=>({url:()=>"https://chatgpt.com/backend-api/f/conversation",status:()=>429,request:()=>({method:()=>method}),headerValue:async()=>"600"});
 response(limited('GET'));await Promise.resolve();
 expect(()=>throwIfConversationRateLimited(page.asPage(),true)).toThrow();
 await navigatePage(page.asPage(),'https://chatgpt.com/');
 expect(()=>throwIfConversationRateLimited(page.asPage())).not.toThrow();
 response(limited('POST'));await Promise.resolve();
 expect(()=>throwIfConversationRateLimited(page.asPage())).toThrow();
});

test("sidebar throttling and an old document's delayed response cannot stop a healthy current chat",async()=>{
 const page=new NavigationPage();page.current='https://chatgpt.com/c/old';
 const handlers=new Map<string,(value:any)=>void>();
 (page as any).on=(name:string,listener:any)=>{handlers.set(name,listener);};
 observeConversationRateLimits(page.asPage());
 const old={url:()=>"https://chatgpt.com/backend-api/conversation/old",method:()=>"GET"};
 handlers.get('request')!(old);page.current='https://chatgpt.com/c/healthy';
 handlers.get('response')!({url:old.url,status:()=>429,request:()=>old,headerValue:async()=>"600"});
 await Promise.resolve();expect(()=>throwIfConversationRateLimited(page.asPage(),true)).not.toThrow();
 const independent=new NavigationPage(),events=new Map<string,(value:any)=>void>();
 (independent as any).on=(name:string,listener:any)=>{events.set(name,listener);};
 observeConversationRateLimits(independent.asPage());
 events.get('response')!({url:()=>"https://chatgpt.com/backend-api/conversations",status:()=>429,headerValue:async()=>"600"});
 await Promise.resolve();await navigatePage(independent.asPage(),'https://chatgpt.com/c/allowed');
 expect(independent.loads).toHaveLength(1);
});

test("server Retry-After survives a browser restart without blocking after expiry",async()=>{
 const directory=mkdtempSync(join(tmpdir(),'codexify-rate-limit-')),path=join(directory,'cooldown.json'),now=Date.now();setSystemTime(now);
 try {
  configureNavigationPersistence(path);
  const page=new NavigationPage();page.responseCode=429;page.retryAfter='600';
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  configureNavigationPersistence();configureNavigationPersistence(path);
  const restarted=new NavigationPage();
  await expect(withConversationCooldownBypass(true,()=>navigatePage(restarted.asPage(),'https://chatgpt.com/c/second'))).rejects.toMatchObject({code:'conversation_cooldown'});
  expect(restarted.loads).toHaveLength(0);
  setSystemTime(now+600_001);await navigatePage(restarted.asPage(),'https://chatgpt.com/c/second');
  expect(restarted.loads).toHaveLength(1);
 }finally{configureNavigationPersistence();unlinkSync(path);rmdirSync(directory);}
});

test("custom navigation settings control normal loads, unavailable probes, and HTTP cooldowns",async()=>{
  configureNavigationTimings(JSON.stringify({conversationLoadIntervalSeconds:7,conversationUnavailableCooldownSeconds:11,rateLimitCooldownSeconds:19}));
  const page=new NavigationPage();
  const now=Date.now();setSystemTime(now);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/first');
  setSystemTime(now+6_999);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/second')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(now+7_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/second');
  recordConversationUnavailable(page.asPage());
  setSystemTime(now+17_999);
  await expect(navigatePage(page.asPage(),page.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(now+18_000);
  page.responseCode=429;
  await expect(navigatePage(page.asPage(),page.current,{reload:true})).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  recordConversationAvailable(page.asPage());page.responseCode=200;
  setSystemTime(now+36_999);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/third')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(now+37_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/third');
  page.responseCode=429;page.retryAfter='40';setSystemTime(now+44_000);
  await expect(navigatePage(page.asPage(),page.current,{reload:true})).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  page.responseCode=200;setSystemTime(now+83_999);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/fourth')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(now+84_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/fourth');
  expect(page.loads).toHaveLength(6);
});

test("navigation timing settings retain partial defaults and reject invalid startup values",()=>{
  configureNavigationTimings('{"conversationLoadIntervalSeconds":86400}');
  for(const raw of ['not json','[]','null','{"conversationLoadIntervalSeconds":0}','{"conversationUnavailableCooldownSeconds":86401}','{"rateLimitCooldownSeconds":1.5}','{"rateLimitCooldownSeconds":"3m"}']){
    expect(()=>configureNavigationTimings(raw)).toThrow();
  }
});

test("same-page reuse makes no request; explicit recovery reloads once",async()=>{
  const page=new NavigationPage();
  await navigatePage(page.asPage(),page.current);expect(page.loads).toEqual([]);
  await navigatePage(page.asPage(),page.current,{reload:true});expect(page.loads).toEqual([page.current]);
});

test("healthy chats share a one-second limit and same-page reuse does not consume a slot",async()=>{
  const page=new NavigationPage();
  await navigatePage(page.asPage(),'https://chatgpt.com/c/first');
  const loadedAt=Date.now();
  await navigatePage(page.asPage(),page.current);
  await expect(navigatePage(page.asPage(),page.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown'});
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/second',{signal:AbortSignal.abort()})).rejects.toThrow();
  expect(page.loads).toEqual(['https://chatgpt.com/c/first']);
  setSystemTime(loadedAt+999);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/second')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(loadedAt+1_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/second');expect(page.loads).toHaveLength(2);
});

test("an eligible recovery never reloads a different page opened by the user",async()=>{
  const page=new NavigationPage();await navigatePage(page.asPage(),'https://chatgpt.com/c/first');
  const target=page.current;page.current='https://chatgpt.com/c/manual';setSystemTime(Date.now()+180_001);
  await expect(navigatePage(page.asPage(),target,{reload:true})).rejects.toMatchObject({code:'external_interference'});expect(page.loads).toHaveLength(1);
});

test("HTTP 429 applies one cooldown across chats and respects Retry-After",async()=>{
  const page=new NavigationPage();page.responseCode=429;page.retryAfter='120';
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  page.responseCode=200;setSystemTime(Date.now()+119_000);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/another')).rejects.toMatchObject({code:'conversation_cooldown'});
  expect(page.loads).toHaveLength(1);
  setSystemTime(Date.now()+61_001);await navigatePage(page.asPage(),'https://chatgpt.com/c/another');expect(page.loads).toHaveLength(2);
});

test("a failed Retry-After read still reports rate limiting and throttles the next load",async()=>{
  const page=new NavigationPage();
  page.responseCode=429;
  page.headerFailure=true;
  await expect(navigatePage(page.asPage(),"https://chatgpt.com/c/first")).rejects.toMatchObject({code:"conversation_load_rate_limited"});
  page.responseCode=200;
  await expect(navigatePage(page.asPage(),"https://chatgpt.com/c/second")).rejects.toMatchObject({code:"conversation_cooldown"});
  expect(page.loads).toHaveLength(1);
});

test("a missing Retry-After has a bounded three-minute cooldown",async()=>{
  const page=new NavigationPage(),now=Date.now();setSystemTime(now);page.responseCode=429;
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  page.responseCode=200;setSystemTime(now+179_999);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/another')).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:now+180_000});
  setSystemTime(now+180_000);await navigatePage(page.asPage(),'https://chatgpt.com/c/another');expect(page.loads).toHaveLength(2);
});

test("a billing-only rate limit does not block unrelated conversation loads",async()=>{
  const page=new NavigationPage();page.responseCode=429;
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/settings/billing')).rejects.toMatchObject({code:'rate_limited'});
  page.responseCode=200;await navigatePage(page.asPage(),'https://chatgpt.com/c/first');expect(page.loads).toHaveLength(2);
});

test("Retry-After HTTP dates are honored and unrelated browser profiles are independent",async()=>{
  const page=new NavigationPage();page.responseCode=429;page.retryAfter=new Date(Date.now()+60_000).toUTCString();
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  const independent=new NavigationPage();await navigatePage(independent.asPage(),'https://chatgpt.com/c/second');expect(independent.loads).toHaveLength(1);
  page.responseCode=200;setSystemTime(Date.now()+180_001);await navigatePage(page.asPage(),'https://chatgpt.com/c/second');expect(page.loads).toHaveLength(2);
});

test("an unavailable chat keeps its five-minute backoff without delaying a different chat",async()=>{
  const first=new NavigationPage(),second=new NavigationPage(first.owner);
  const failedAt=Date.now();
  first.current='https://chatgpt.com/c/failed';recordConversationUnavailable(first.asPage());
  await navigatePage(first.asPage(),first.current);expect(first.loads).toEqual([]);
  await expect(navigatePage(first.asPage(),first.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown'});
  await navigatePage(second.asPage(),'https://chatgpt.com/g/project/c/second');
  expect(second.loads).toHaveLength(1);
  recordConversationAvailable(second.asPage());
  setSystemTime(failedAt+299_000);
  await expect(navigatePage(first.asPage(),first.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:failedAt+300_000});
  setSystemTime(failedAt+300_001);
  const results=await Promise.allSettled([
    navigatePage(first.asPage(),first.current,{reload:true}),
    navigatePage(second.asPage(),'https://chatgpt.com/c/another-healthy'),
  ]);
  expect(results[0]!.status).toBe('fulfilled');
  expect(results[1]).toMatchObject({status:'rejected',reason:{code:'conversation_cooldown'}});
  expect(first.loads).toHaveLength(1);expect(second.loads).toHaveLength(1);
  const probedAt=Date.now();
  recordConversationUnavailable(first.asPage());
  setSystemTime(probedAt+1_000);
  await navigatePage(second.asPage(),'https://chatgpt.com/c/another-healthy');recordConversationAvailable(second.asPage());
  setSystemTime(probedAt+299_000);
  await expect(navigatePage(first.asPage(),first.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:probedAt+300_000});
  setSystemTime(probedAt+300_001);
  await navigatePage(first.asPage(),first.current,{reload:true});expect(first.loads).toHaveLength(2);
});

test("health polling does not postpone a failed conversation's recovery slot even after expiry",async()=>{
  const page=new NavigationPage(),now=Date.now();setSystemTime(now);page.current='https://chatgpt.com/c/recovery';recordConversationUnavailable(page.asPage());
  setSystemTime(now+299_000);recordConversationUnavailable(page.asPage());
  await expect(navigatePage(page.asPage(),page.current,{reload:true})).rejects.toMatchObject({availableAtMs:now+300_000});
  setSystemTime(now+300_000);recordConversationUnavailable(page.asPage());
  await navigatePage(page.asPage(),page.current,{reload:true});expect(page.loads).toHaveLength(1);
});

test("only the same verified conversation clears its deadline; settings and healthy other chats cannot",async()=>{
  const page=new NavigationPage(),now=Date.now();setSystemTime(now);page.current='https://chatgpt.com/c/blocked';recordConversationUnavailable(page.asPage());
  await navigatePage(page.asPage(),'https://chatgpt.com/settings/billing');recordConversationAvailable(page.asPage());
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/blocked')).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:now+300_000});
  const independent=new NavigationPage();await navigatePage(independent.asPage(),'https://chatgpt.com/c/independent');
  await navigatePage(page.asPage(),'https://chatgpt.com/c/healthy');recordConversationAvailable(page.asPage());
  setSystemTime(now+1_000);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/blocked')).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:now+300_000});
  page.current='https://chatgpt.com/c/blocked';recordConversationAvailable(page.asPage());
  await navigatePage(page.asPage(),page.current,{reload:true});expect(page.loads).toHaveLength(3);
});

test("project, query and trailing-slash aliases share one conversation backoff",async()=>{
  const page=new NavigationPage(),now=Date.now();setSystemTime(now);page.current='https://chatgpt.com/g/project-a/c/blocked/?model=fixture';recordConversationUnavailable(page.asPage());
  for(const url of ['https://chatgpt.com/c/blocked','https://chatgpt.com/g/project-b/c/blocked/','https://chatgpt.com/c/blocked?model=other'])
    await expect(navigatePage(page.asPage(),url)).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:now+300_000});
  page.current='https://chatgpt.com/c/blocked';recordConversationAvailable(page.asPage());
  await navigatePage(page.asPage(),'https://chatgpt.com/g/project-b/c/blocked/');expect(page.loads).toHaveLength(1);
});

test("observing a non-conversation failure never assigns a cooldown to future chats",async()=>{
  const page=new NavigationPage();recordConversationUnavailable(page.asPage());
  await navigatePage(page.asPage(),'https://chatgpt.com/c/healthy');expect(page.loads).toHaveLength(1);
});

test("the conversation cooldown never shortens a longer HTTP Retry-After",async()=>{
  const page=new NavigationPage();page.responseCode=429;page.retryAfter='600';
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  recordConversationUnavailable(page.asPage());setSystemTime(Date.now()+300_000);page.responseCode=200;
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/next')).rejects.toMatchObject({code:'conversation_cooldown'});
  expect(page.loads).toHaveLength(1);
  setSystemTime(Date.now()+300_000);await navigatePage(page.asPage(),'https://chatgpt.com/c/next');expect(page.loads).toHaveLength(2);
});

test("a failure observed during an in-flight load blocks the next queued document request",async()=>{
  const page=new NavigationPage(),goto=page.goto.bind(page);
  let opened!:()=>void,release!:()=>void;
  const started=new Promise<void>(resolve=>{opened=resolve;}),held=new Promise<void>(resolve=>{release=resolve;});
  page.goto=async url=>{const response=await goto(url);opened();await held;return response;};
  const first=navigatePage(page.asPage(),'https://chatgpt.com/c/first');await started;
  const next=navigatePage(page.asPage(),'https://chatgpt.com/c/next');recordConversationUnavailable(page.asPage());release();await first;
  await expect(next).rejects.toMatchObject({code:'conversation_cooldown'});expect(page.loads).toHaveLength(1);
  setSystemTime(Date.now()+1_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/next');expect(page.loads).toHaveLength(2);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_cooldown'});
});

test("cancelling a conversation probe does not consume its five-minute slot",async()=>{
  const page=new NavigationPage();page.current='https://chatgpt.com/c/probe';recordConversationUnavailable(page.asPage());setSystemTime(Date.now()+300_000);
  await expect(navigatePage(page.asPage(),page.current,{reload:true,signal:AbortSignal.abort()})).rejects.toThrow();
  expect(page.loads).toEqual([]);
  await navigatePage(page.asPage(),page.current,{reload:true});expect(page.loads).toHaveLength(1);
});

test("deferral carries the latest applicable absolute deadline without consuming or extending a slot",async()=>{
  const now=Date.now();setSystemTime(now);
  const page=new NavigationPage();page.responseCode=429;page.retryAfter='600';
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  recordConversationUnavailable(page.asPage());page.responseCode=200;
  for(const elapsed of [1_000,179_000,300_000,599_999]){
    setSystemTime(now+elapsed);
    await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/second')).rejects.toMatchObject({code:'conversation_cooldown',availableAtMs:now+600_000});
    expect(page.loads).toHaveLength(1);
  }
  setSystemTime(now+600_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/second');
  expect(page.loads).toHaveLength(2);
});
