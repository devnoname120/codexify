import {expect,test} from "bun:test";
import {observeReadOnly,RendererProbeGate} from "../src/observation";
import {ChatGptBrowserDriver} from "../src/chatgpt-driver";
import {SingleWindowCoordinator} from "../src/browser-window";
import {protocolBuild} from "../src/protocol";
import {moveChatGptEffortSlider} from "../src/session";

test("read-only observations cancel without waiting for an unresponsive renderer",async()=>{
 const signal=new AbortController();let finish!:(value:number)=>void;
 const late=new Promise<number>(resolve=>finish=resolve);
 const observed=observeReadOnly(late,signal.signal,1000);signal.abort(new Error("expected cancellation"));
 await expect(observed).rejects.toThrow("expected cancellation");finish(1);
 await expect(observeReadOnly(new Promise(()=>{}),undefined,10)).rejects.toThrow("browser and draft were preserved");
});

test("renderer liveness requires repeated probes over a minute and resets on actual recovery",()=>{
 const gate=new RendererProbeGate();expect(gate.record(false,0)).toBe(true);
 expect(gate.record(false,1000)).toBe(true);expect(gate.record(false,59000)).toBe(true);
 expect(gate.record(false,61000)).toBe(false);expect(gate.record(true,62000)).toBe(true);
 expect(gate.record(false,63000)).toBe(true);
});

test("cancelled handoff observation releases ownership without closing or navigating the browser",async()=>{
 let closed=0;const pending=new Promise(()=>{});
 const page={url:()=>"https://chatgpt.com/c/observation",isClosed:()=>false,evaluate:()=>pending,frames:()=>[],locator:()=>({filter:()=>({count:()=>pending})})};
 const surface={page:async()=>page,beginAction:async()=>{},endAction:async()=>{},close:async()=>{closed++;},navigate:async()=>{throw new Error("Unexpected navigation");},reveal:async()=>{}};
 const driver=new ChatGptBrowserDriver(surface as any,protocolBuild("observation-test"));
 const coordinator=new SingleWindowCoordinator(driver,protocolBuild("observation-test"),{operationMs:30,cleanupMs:100,hardStopMs:20});
 await expect(coordinator.turnHandoffComplete(page.url(),"account","renew-"+"a".repeat(64))).rejects.toThrow();
 expect(closed).toBe(0);expect((await coordinator.liveness()).available).toBe(true);
});

test("stalled connector account observation releases maintenance without restarting or mutating the browser",async()=>{
 let closed=0,navigated=0;const pending=new Promise(()=>{});
 const page={url:()=>"https://chatgpt.com/c/maintenance",isClosed:()=>false,evaluate:()=>pending};
 const surface={page:async()=>page,beginAction:async()=>{},endAction:async()=>{},close:async()=>{closed++;},navigate:async()=>{navigated++;return page;},reveal:async()=>{}};
 const driver=new ChatGptBrowserDriver(surface as any,protocolBuild("observation-test"));
 const coordinator=new SingleWindowCoordinator(driver,protocolBuild("observation-test"),{operationMs:30,cleanupMs:100,hardStopMs:20});
 const cancellation=new AbortController();
 const refresh=coordinator.refreshConnector("Fixture connector",cancellation.signal);
 setTimeout(()=>cancellation.abort(),20);
 await expect(refresh).rejects.toThrow();
 expect(closed).toBe(0);expect(navigated).toBe(0);
 expect((await coordinator.liveness()).available).toBe(true);
});

test("a cancelled effort observation cannot move the slider after its late read settles",async()=>{
 let finish!:(value:unknown)=>void,presses=0;
 const pending=new Promise(resolve=>finish=resolve);
 const locator={evaluate:()=>pending,locator:()=>({press:async()=>{presses++;}})};
 const controller=new AbortController();
 const move=moveChatGptEffortSlider({sliderContainer:locator,slider:locator} as any,1,controller.signal);
 controller.abort(new Error("cancelled effort setup"));
 await expect(move).rejects.toThrow("cancelled effort setup");
 finish({min:"0",max:"1",value:"0",locks:["false","false"],labels:["Instant","Medium"]});
 await new Promise(resolve=>setTimeout(resolve,0));
 expect(presses).toBe(0);
});
