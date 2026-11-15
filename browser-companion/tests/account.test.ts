import { expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import type { Page } from "patchright";
import { HealthAccountCache, HEALTH_ACCOUNT_TTL_MS, HEALTH_ACCOUNT_FAILURE_TTL_MS, readChatGptAccount, fingerprintAccountIdentity, validateAccountSessionProjection } from "../src/account";

test("account fingerprint is stable, private, and changes across user or account", () => {
  const first = fingerprintAccountIdentity("user-private", "account-private");
  expect(first).toMatch(/^[0-9a-f]{64}$/);
  expect(first).not.toContain("user-private");
  expect(first).toBe(fingerprintAccountIdentity("user-private", "account-private"));
  expect(first).not.toBe(fingerprintAccountIdentity("other-user", "account-private"));
  expect(first).not.toBe(fingerprintAccountIdentity("user-private", "other-account"));
});

test("session projection accepts only the bounded identity fields used by Codexify", () => {
  expect(validateAccountSessionProjection({
    userId:"user-1", accountId:"account-1", planType:"pro", structure:"personal", needsAttention:false,
  })).toEqual({
    userId:"user-1", accountId:"account-1", planType:"pro", structure:"personal", needsAttention:false,
  });
  for (const invalid of [
    { userId:"", accountId:"a", planType:"pro", structure:"personal", needsAttention:false },
    { userId:"u", accountId:"a", planType:"x".repeat(257), structure:"personal", needsAttention:false },
    { userId:"u", accountId:"a", planType:"pro", structure:"personal", needsAttention:"no" },
    { userId:"u", accountId:"a", planType:"pro", structure:"personal", email:"private@example.com", needsAttention:false },
  ]) expect(() => validateAccountSessionProjection(invalid)).toThrow();
});

class AccountPage extends EventEmitter {
  requests=0; location="https://chatgpt.com/c/fixture"; closed=false;
  readonly frame={}; result={userId:"fixture-user",accountId:"fixture-account",planType:"pro",structure:"personal",needsAttention:false};
  failure:Error|undefined; gate:Promise<void>|undefined;
  url=()=>this.location; isClosed=()=>this.closed; mainFrame=()=>this.frame;
  async evaluate(){this.requests++;if(this.gate)await this.gate;if(this.failure)throw this.failure;return this.result;}
  asPage(){return this as unknown as Page;}
}

test("passive health polling coalesces requests and refreshes only when the account TTL expires",async()=>{
  const page=new AccountPage();let now=0;const cache=new HealthAccountCache(()=>now);let release!:()=>void;
  page.gate=new Promise(resolve=>{release=resolve;});
  const reads=Array.from({length:20},()=>cache.read(page.asPage()));expect(page.requests).toBe(1);release();
  const accounts=await Promise.all(reads);expect(new Set(accounts.map(account=>account.fingerprint)).size).toBe(1);
  for(let i=0;i<40;i++){now=HEALTH_ACCOUNT_TTL_MS-1;await cache.read(page.asPage());}expect(page.requests).toBe(1);
  now=HEALTH_ACCOUNT_TTL_MS;await cache.read(page.asPage());expect(page.requests).toBe(2);
  cache.dispose();expect(page.listenerCount("framenavigated")).toBe(0);expect(page.listenerCount("close")).toBe(0);
});

test("page navigation, reload, closure, and explicit invalidation discard the passive cache",async()=>{
  const page=new AccountPage(),cache=new HealthAccountCache();await cache.read(page.asPage());
  page.emit("framenavigated",{});await cache.read(page.asPage());expect(page.requests,'subframe navigation does not fetch another session').toBe(1);
  page.emit("framenavigated",page.frame);await cache.read(page.asPage());expect(page.requests,'same-URL main frame reload refreshes identity').toBe(2);
  page.location='https://chatgpt.com/c/other';await cache.read(page.asPage());expect(page.requests).toBe(3);
  cache.invalidate();await cache.read(page.asPage());expect(page.requests).toBe(4);
  page.closed=true;page.emit("close");await expect(cache.read(page.asPage())).rejects.toMatchObject({code:'external_interference'});expect(page.requests).toBe(4);
  const replacement=new AccountPage();await cache.read(replacement.asPage());expect(replacement.requests).toBe(1);expect(page.listenerCount("framenavigated")).toBe(0);
  cache.dispose();
});

test("a late session reply cannot populate the cache for a new page document",async()=>{
  const page=new AccountPage(),cache=new HealthAccountCache();let release!:()=>void;page.gate=new Promise(resolve=>{release=resolve;});
  const read=cache.read(page.asPage());page.emit("framenavigated",page.frame);release();
  await expect(read).rejects.toMatchObject({code:'external_interference'});await cache.read(page.asPage());expect(page.requests).toBe(2);cache.dispose();
});

test("failed health refreshes are throttled without serving a stale successful identity",async()=>{
  const page=new AccountPage();let now=0;const cache=new HealthAccountCache(()=>now);await cache.read(page.asPage());
  page.failure=new Error('Session could not be verified');now=HEALTH_ACCOUNT_TTL_MS;
  for(let i=0;i<10;i++)await expect(cache.read(page.asPage())).rejects.toThrow('Session could not be verified');expect(page.requests).toBe(2);
  now+=HEALTH_ACCOUNT_FAILURE_TTL_MS;page.failure=undefined;await cache.read(page.asPage());expect(page.requests).toBe(3);cache.dispose();
});

test("explicit action account reads remain fresh while the passive cache is populated",async()=>{
  const page=new AccountPage(),cache=new HealthAccountCache();const before=await cache.read(page.asPage());page.result.accountId='new-account';
  expect((await cache.read(page.asPage())).fingerprint).toBe(before.fingerprint);
  for(let i=0;i<3;i++)expect((await readChatGptAccount(page.asPage())).fingerprint).not.toBe(before.fingerprint);
  expect(page.requests).toBe(4);cache.dispose();
});

test("confirmed missing sessions retain authentication-required errors through the health cache",async()=>{
  const page=new AccountPage(),cache=new HealthAccountCache();
  page.evaluate=async()=>{page.requests++;return null as unknown as typeof page.result;};
  await expect(readChatGptAccount(page.asPage())).rejects.toMatchObject({code:'authentication_required'});
  await expect(cache.read(page.asPage())).rejects.toMatchObject({code:'authentication_required'});
  await expect(cache.read(page.asPage())).rejects.toMatchObject({code:'authentication_required'});
  expect(page.requests).toBe(2);
  cache.dispose();
});
