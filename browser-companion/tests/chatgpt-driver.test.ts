import { expect, test } from "bun:test";
import type { Page } from "patchright";
import {
  ChatGptBrowserDriver,
  type SingleBrowserSurface,
} from "../src/chatgpt-driver";
import { BrowserCompanionError } from "../src/errors";
import { protocolBuild } from "../src/protocol";

class UnavailableSurface implements SingleBrowserSurface {
  async page(): Promise<Page> {
    throw new BrowserCompanionError("The shared ChatGPT window was destroyed", "external_interference");
  }

  async navigate(): Promise<Page> {
    return this.page();
  }

  async reveal(): Promise<void> {}
  async beginAction(): Promise<void> {}
  async endAction(): Promise<void> {}
  async close(): Promise<void> {}
}

test("browser infrastructure failures are unavailable rather than authentication failures", async () => {
  const driver = new ChatGptBrowserDriver(new UnavailableSurface(), protocolBuild("test-build"));

  await expect(driver.health()).resolves.toEqual({
    authenticated:false,
    accountFingerprint:null,
    status:"unavailable",
    detail:"The shared ChatGPT window was destroyed",
  });
});

test('a never-settling page observation cannot poison every future health check',async()=>{
  let reads=0;
  class HungOnceSurface extends UnavailableSurface {
    override async page():Promise<Page>{
      if(++reads===1)return new Promise(()=>{});
      return {isClosed:()=>false,url:()=> 'https://chatgpt.com/auth/login'} as unknown as Page;
    }
  }
  const driver=new ChatGptBrowserDriver(new HungOnceSurface(),protocolBuild('health-recovery'));
  const first=await driver.health();expect(['busy','unavailable']).toContain(first.status);
  await new Promise(resolve=>setTimeout(resolve,20));
  const recovered=await driver.health();expect(recovered.status).toBe('authentication_required');expect(reads).toBe(2);
},10_000);


test("a Chrome page closed during health inspection is unavailable rather than signed out",async()=>{
  class ClosingSurface extends UnavailableSurface {
    override async page():Promise<Page>{return {isClosed:()=>true,locator:()=>{throw new Error("Target closed");}} as unknown as Page;}
  }
  const driver=new ChatGptBrowserDriver(new ClosingSurface(),protocolBuild("test-build"));
  const health=await driver.health();expect(health.status).toBe("unavailable");expect(health.authenticated).toBeFalse();
  expect(health.detail).not.toContain("authentication");
});

test("startup plan lookup uses the verified session without navigating or touching the shared editor",async()=>{
  let accountId="fixture-account",planType="pro",sessionReads=0;
  const projection=()=>({userId:"fixture-user",accountId,planType,structure:"personal",needsAttention:false});
  const page={
    url:()=>"https://chatgpt.com/c/preserved-conversation",
    evaluate:async()=>{sessionReads++;return projection();},
    locator:()=>{throw new Error("Passive plan lookup must not inspect or change the UI");},
  } as unknown as Page;
  class PassiveSurface extends UnavailableSurface {
    override async page(){return page;}
    override async navigate():Promise<Page>{throw new Error("Passive plan lookup must not navigate");}
    override async beginAction():Promise<void>{throw new Error("Passive plan lookup must not acquire an editing action");}
  }
  const driver=new ChatGptBrowserDriver(new PassiveSurface(),protocolBuild("test-build"));
  const first=await driver.accountPlan("startup");
  expect(first).toMatchObject({label:"Pro",sourceLabel:"pro",kind:"pro"});
  expect(await driver.accountPlan("startup")).toEqual(first);
  expect(sessionReads).toBe(2);
  planType="free";
  expect(await driver.accountPlan("startup")).toEqual({...first,label:"Free",sourceLabel:"free",kind:"free"});
  planType="pro";
  accountId="different-account";
  expect((await driver.accountPlan("startup")).accountFingerprint).not.toBe(first.accountFingerprint);
  const cancelled=new AbortController();cancelled.abort();
  await expect(driver.accountPlan("cancelled",cancelled.signal)).rejects.toMatchObject({name:"AbortError"});
  expect(sessionReads).toBe(4);
});
