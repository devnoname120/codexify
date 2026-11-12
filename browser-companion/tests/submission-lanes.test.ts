import {afterEach,expect,setSystemTime,test} from "bun:test";
import type {BrowserContext,Page} from "patchright";
import {acquireSubmissionSlot,configureNavigationPersistence,configureNavigationTimings,navigatePage,observeConversationRateLimits} from "../src/navigation";

class PageStub {
  current="https://chatgpt.com/";
  loads:string[]=[];
  beforeLoad?:()=>Promise<void>;
  callbacks=new Map<string,Function>();
  constructor(readonly owner={} as BrowserContext){}
  context=()=>this.owner;
  url=()=>this.current;
  on=(event:string,callback:Function)=>{this.callbacks.set(event,callback);};
  async goto(url:string){this.current=url;this.loads.push(url);await this.beforeLoad?.();return {status:()=>200};}
  asPage=()=>this as unknown as Page;
  rateLimitSubmission(){this.callbacks.get("response")?.({status:()=>429,url:()=>"https://chatgpt.com/backend-api/conversation",
    request:()=>({method:()=>"POST"}),headerValue:async()=>"120"});}
}
afterEach(()=>{setSystemTime();configureNavigationTimings();configureNavigationPersistence();});

test("a stalled document does not retain another page's navigation reservation",async()=>{
  const first=new PageStub(),second=new PageStub(first.owner),now=Date.now();setSystemTime(now);
  let finish!:()=>void;first.beforeLoad=()=>new Promise<void>(resolve=>{finish=resolve;});
  const pending=navigatePage(first.asPage(),"https://chatgpt.com/c/first");
  await Promise.resolve();await Promise.resolve();
  expect(first.loads).toHaveLength(1);
  setSystemTime(now+1001);
  try {await navigatePage(second.asPage(),"https://chatgpt.com/c/second");expect(second.loads).toHaveLength(1);}
  finally{finish();await pending;}
});

test("actual Send boundaries across pages retain one second spacing",async()=>{
  const first=new PageStub(),second=new PageStub(first.owner);
  const a=await acquireSubmissionSlot(first.asPage());a.markSent();const firstAt=Date.now();a.release();
  const b=await acquireSubmissionSlot(second.asPage());
  try {b.markSent();expect(Date.now()-firstAt).toBeGreaterThanOrEqual(990);}finally{b.release();}
},5000);

test("a cancelled queued Send cannot steal or release another page's lease",async()=>{
  const first=new PageStub(),second=new PageStub(first.owner),third=new PageStub(first.owner);
  const a=await acquireSubmissionSlot(first.asPage()),cancel=new AbortController();
  const waiting=acquireSubmissionSlot(second.asPage(),cancel.signal);cancel.abort();await expect(waiting).rejects.toThrow();
  let acquired=false;const next=acquireSubmissionSlot(third.asPage()).then(slot=>{acquired=true;return slot;});
  await Promise.resolve();await Promise.resolve();expect(acquired).toBe(false);
  a.release();const c=await next;expect(acquired).toBe(true);c.release();
});

test("a server429 arriving while another page holds Send blocks the next page",async()=>{
  const first=new PageStub(),second=new PageStub(first.owner);observeConversationRateLimits(first.asPage());
  const a=await acquireSubmissionSlot(first.asPage());
  const next=acquireSubmissionSlot(second.asPage());first.rateLimitSubmission();a.release();
  await expect(next).rejects.toMatchObject({code:"rate_limited"});
});

test("a server429 after preflight still blocks the final irreversible boundary",async()=>{
  const page=new PageStub();observeConversationRateLimits(page.asPage());
  const lease=await acquireSubmissionSlot(page.asPage());page.rateLimitSubmission();
  try {expect(()=>lease.markSent()).toThrow();}finally{lease.release();}
});
