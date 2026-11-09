import {afterEach,expect,setSystemTime,test} from "bun:test";
import type {BrowserContext,Page} from "patchright";
import {configureNavigationTimings,navigatePage,recordConversationAvailable,recordConversationUnavailable,withConversationCooldownBypass,conversationLoadAvailableAt} from "../src/navigation";

class NavigationPage {
  current="https://chatgpt.com/";
  loads:string[]=[];
  responseCode=200;
  retryAfter:string|null=null;
  constructor(readonly owner={} as BrowserContext){}
  context=()=>this.owner;
  url=()=>this.current;
  async goto(url:string){this.current=url;this.loads.push(url);return {status:()=>this.responseCode,headerValue:async()=>this.retryAfter};}
  reload=()=>this.goto(this.current);
  asPage=()=>this as unknown as Page;
}
afterEach(()=>{setSystemTime();configureNavigationTimings();});

test("passive availability reports the real shared deadline without loading or delaying current-page reuse",async()=>{
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
  await withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),'https://chatgpt.com/c/manual-again'));
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/automatic')).rejects.toMatchObject({code:'conversation_cooldown'});
  await expect(withConversationCooldownBypass(true,()=>navigatePage(page.asPage(),'https://chatgpt.com/c/cancelled',{signal:AbortSignal.abort()}))).rejects.toThrow();
  expect(page.loads).toHaveLength(5);
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

test("different chats share a three-minute limit and same-page reuse does not consume a slot",async()=>{
  const page=new NavigationPage();
  await navigatePage(page.asPage(),'https://chatgpt.com/c/first');
  const loadedAt=Date.now();
  await navigatePage(page.asPage(),page.current);
  await expect(navigatePage(page.asPage(),page.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown'});
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/second',{signal:AbortSignal.abort()})).rejects.toThrow();
  expect(page.loads).toEqual(['https://chatgpt.com/c/first']);
  setSystemTime(loadedAt+179_000);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/second')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(loadedAt+180_001);
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

test("a missing Retry-After has a bounded three-minute cooldown",async()=>{
  const page=new NavigationPage();page.responseCode=429;
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/first')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  page.responseCode=200;setSystemTime(Date.now()+180_001);await navigatePage(page.asPage(),'https://chatgpt.com/c/another');expect(page.loads).toHaveLength(2);
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

test("conversation access failures allow one load per five minutes across chats and reloads",async()=>{
  const first=new NavigationPage(),second=new NavigationPage(first.owner);
  const failedAt=Date.now();
  first.current='https://chatgpt.com/c/failed';recordConversationUnavailable(first.asPage());
  await navigatePage(first.asPage(),first.current);expect(first.loads).toEqual([]);
  await expect(navigatePage(first.asPage(),first.current,{reload:true})).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(failedAt+299_000);
  await expect(navigatePage(second.asPage(),'https://chatgpt.com/g/project/c/second')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(failedAt+300_001);
  const results=await Promise.allSettled([
    navigatePage(first.asPage(),first.current,{reload:true}),
    navigatePage(second.asPage(),'https://chatgpt.com/g/project/c/second'),
  ]);
  expect(results[0]!.status).toBe('fulfilled');
  expect(results[1]).toMatchObject({status:'rejected',reason:{code:'conversation_cooldown'}});
  expect(first.loads).toHaveLength(1);expect(second.loads).toEqual([]);
  const probedAt=Date.now();
  recordConversationUnavailable(first.asPage());
  setSystemTime(probedAt+299_000);
  await expect(navigatePage(second.asPage(),'https://chatgpt.com/c/second')).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(probedAt+300_001);
  await navigatePage(second.asPage(),'https://chatgpt.com/c/second');expect(second.loads).toHaveLength(1);
});

test("health polling does not postpone the shared conversation recovery slot",async()=>{
  const page=new NavigationPage();recordConversationUnavailable(page.asPage());
  setSystemTime(Date.now()+299_000);recordConversationUnavailable(page.asPage());
  setSystemTime(Date.now()+1_000);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/recovery');expect(page.loads).toHaveLength(1);
});

test("only a verified conversation clears the cooldown; account settings and other profiles remain available",async()=>{
  const page=new NavigationPage();recordConversationUnavailable(page.asPage());
  await navigatePage(page.asPage(),'https://chatgpt.com/settings/billing');recordConversationAvailable(page.asPage());
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/blocked')).rejects.toMatchObject({code:'conversation_cooldown'});
  const independent=new NavigationPage();await navigatePage(independent.asPage(),'https://chatgpt.com/c/independent');
  page.current='https://chatgpt.com/c/healthy';recordConversationAvailable(page.asPage());
  await navigatePage(page.asPage(),'https://chatgpt.com/c/recovered');expect(page.loads).toHaveLength(2);
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
});

test("cancelling a conversation probe does not consume its five-minute slot",async()=>{
  const page=new NavigationPage();recordConversationUnavailable(page.asPage());setSystemTime(Date.now()+300_000);
  await expect(navigatePage(page.asPage(),'https://chatgpt.com/c/cancelled',{signal:AbortSignal.abort()})).rejects.toThrow();
  expect(page.loads).toEqual([]);
  await navigatePage(page.asPage(),'https://chatgpt.com/c/probe');expect(page.loads).toHaveLength(1);
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
