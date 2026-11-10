import { chmod, lstat, readFile, symlink } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { mkdtemp } from "node:fs/promises";
import { expect, test } from "bun:test";
import type { CapabilityCatalog, ConversationApprovalRequest, ExecuteActionRequest } from "../src/protocol";
import { SingleWindowCoordinator, type BrowserDriver, type DriverExecution } from "../src/browser-window";
import { startControlServer } from "../src/control-server";
import { protocolBuild } from "../src/protocol";
import {browserStage} from "../src/errors";

const TEST_BUILD = protocolBuild("test-build");

test("navigation availability is authenticated and read-only while a browser action owns the tab",async()=>{
 class AvailabilityDriver extends BlockingDriver {
  reads:string[]=[];
  async navigationAvailability(url:string){this.reads.push(url);return 180_000;}
 }
 const root=await mkdtemp(join(tmpdir(),"browser-availability-")),driver=new AvailabilityDriver();
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD),server=await startControlServer(coordinator,join(root,"descriptor.json"));
 try {
  const headers={Authorization:`Bearer ${server.token}`,"Content-Type":"application/json"},url="https://chatgpt.com/c/next";
  expect((await fetch(`${server.endpoint}/v1/navigation/availability`,{method:"POST",body:JSON.stringify({url})})).status).toBe(401);
  await coordinator.execute(executeBody());
  const deadline=Date.now()+2000;
  while(!(await coordinator.action("control-action"))?.sendAccepted){expect(Date.now()).toBeLessThan(deadline);await Bun.sleep(1);}
  const result=await fetch(`${server.endpoint}/v1/navigation/availability`,{method:"POST",headers,body:JSON.stringify({url})});
  expect(await result.json()).toEqual({protocolVersion:1,build:TEST_BUILD,availableAtMs:180_000});
  expect(driver.reads).toEqual([url]);expect((await coordinator.health()).activeActionId).toBe("control-action");
  expect((await fetch(`${server.endpoint}/v1/navigation/availability`,{method:"POST",headers,body:JSON.stringify({url:"http://example.test"})})).status).toBe(400);
  expect(driver.reads).toEqual([url]);
 }finally{await server.close();}
});

class ImmediateDriver implements BrowserDriver {
  async health() {
    return {
      authenticated:true,
      accountFingerprint:"account-test",
      status:"idle" as const,
      detail:null,
    };
  }
  async login() { return this.health(); }
  async capabilities(): Promise<CapabilityCatalog> {
    return {
      protocolVersion:1, build:TEST_BUILD, accountFingerprint:"account-test", catalogRevision:"catalog-test",
      models:[{ id:"gpt-6-sol", label:"GPT-6 Sol", efforts:[{ id:"high", label:"High", available:true, reason:null }] }],
    };
  }
  async reveal() {}
  async execute(execution: DriverExecution) {
    execution.emit({ type:"navigating" });
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({ type:"answer_delta", markdown:"Done", continuation:false });
    execution.emit({ type:"generation_complete", finalMarkdown:"Done", canonicalUrl:execution.request.expectedUrl });
  }
}

class BlockingDriver extends ImmediateDriver {
  override async execute(execution: DriverExecution) {
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    await new Promise<void>((_resolve, reject) => {
      execution.signal.addEventListener(
        "abort",
        () => reject(new DOMException("aborted", "AbortError")),
        { once:true },
      );
    });
  }
}

test("pairing lease renewal is authenticated and preserves the held page",async()=>{
  class PairDriver extends ImmediateDriver {
    override async execute(execution:DriverExecution){
      execution.emit({type:'send_accepted'});
      execution.emit({type:'conversation_created',canonicalUrl:'https://chatgpt.com/c/held'});
      execution.emit({type:'submission_complete',canonicalUrl:'https://chatgpt.com/c/held'});
    }
  }
  const root=await mkdtemp(join(tmpdir(),'pair-lease-control-'));
  const coordinator=new SingleWindowCoordinator(new PairDriver(),TEST_BUILD);
  const server=await startControlServer(coordinator,join(root,'descriptor.json'));
  try{
    const request:ExecuteActionRequest={...executeBody(),url:'https://chatgpt.com/new',expectedUrl:'https://chatgpt.com/new',model:null,effort:null,retainPairingWindow:true,prompt:{kind:'create_chat',continuationToken:'A'.repeat(43),reference:Buffer.alloc(32,1).toString('base64url'),setupReference:null}};
    await coordinator.execute(request);for await(const _ of coordinator.events(request.actionId,0)){}
    const endpoint=server.endpoint+'/v1/actions/refresh-pairing',body=JSON.stringify({actionId:request.actionId});
    expect((await fetch(endpoint,{method:'POST',body})).status).toBe(401);
    const headers={Authorization:`Bearer ${server.token}`,'Content-Type':'application/json'};
    expect(await(await fetch(endpoint,{method:'POST',headers,body})).json()).toEqual({retained:true});
    expect((await coordinator.health()).activeActionId).toBe(request.actionId);
    expect(await(await fetch(endpoint,{method:'POST',headers,body:JSON.stringify({actionId:'other'})})).json()).toEqual({retained:false});
  }finally{await server.close();}
});

class PrivateFailureDriver extends ImmediateDriver {
  override async capabilities(): Promise<CapabilityCatalog> {
    throw new Error(
      "capability probe failed at https://chatgpt.com/c/private-conversation with private page text",
    );
  }

  override async reveal() {
    throw new Error(
      "reveal failed at https://chatgpt.com/c/private-conversation with private browser prompt",
    );
  }
}

test("controlled picker failures return a structured diagnosis instead of an unexplained conflict",async()=>{
  class PickerFailureDriver extends ImmediateDriver {
    override async capabilities():Promise<CapabilityCatalog> {
      return browserStage("model_options","ChatGPT did not expose its model options",async()=>{
        throw new Error("private page text at https://chatgpt.com/c/private-conversation");
      });
    }
  }
  const root=await mkdtemp(join(tmpdir(),"picker-error-control-"));
  const server=await startControlServer(new SingleWindowCoordinator(new PickerFailureDriver(),TEST_BUILD),join(root,"descriptor.json"));
  try {
    const response=await fetch(`${server.endpoint}/v1/session/capabilities`,{method:"POST",headers:{Authorization:`Bearer ${server.token}`,"Content-Type":"application/json"},body:"{}"});
    expect(response.status).toBe(409);
    const value=await response.json();
    expect(value).toEqual({protocolVersion:1,build:TEST_BUILD,error:{code:"selector_drift",phase:"model_options",message:"ChatGPT did not expose its model options (step: model_options)"}});
    expect(JSON.stringify(value)).not.toContain("private");
  }finally{await server.close();}
});

function executeBody(): ExecuteActionRequest {
  return {
    actionId:"control-action",
    idempotencyKey:"control-execute",
    url:"https://chatgpt.com/c/control",
    pairingGeneration:1,
    connectorName:"Codexify",
    expectedAccountFingerprint:"account-test",
    model:"gpt-6-sol",
    effort:"high",
    prompt:{ kind:"offline_chat", batchId:"batch-control", messages:[{ id:"message-control", markdown:"Continue." }] },
    expectedUrl:"https://chatgpt.com/c/control",
    autoApproveToolCalls:false,
  };
}

test("execute acknowledges and retains an action before slow account preflight", async () => {
  let release!: () => void;
  const gate = new Promise<void>(resolve => { release = resolve; });
  class SlowPreflightDriver extends ImmediateDriver {
    runs = 0;
    override async health() { await gate; return super.health(); }
    override async execute(execution: DriverExecution) { this.runs++; await super.execute(execution); }
  }
  const root = await mkdtemp(join(tmpdir(), "browser-admission-"));
  const driver = new SlowPreflightDriver(), coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const server = await startControlServer(coordinator, join(root, "descriptor.json"));
  const headers = { Authorization:`Bearer ${server.token}`, "Content-Type":"application/json" };
  try {
    const post = () => fetch(`${server.endpoint}/v1/actions/execute`, {
      method:"POST", headers, body:JSON.stringify(executeBody()), signal:AbortSignal.timeout(250),
    });
    const response = await post();
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({state:"queued",sendAccepted:false,lastEventSequence:1});
    expect(await (await fetch(`${server.endpoint}/v1/actions/control-action`,{headers})).json())
      .toMatchObject({state:"queued",sendAccepted:false});
    expect((await post()).status).toBe(200); // A lost acknowledgement cannot enqueue a second Send.
    expect(driver.runs).toBe(0);
    release();
    const stream = await fetch(`${server.endpoint}/v1/actions/control-action/events?after=0`,{headers});
    const events = (await stream.text()).trim().split("\n").map(line=>JSON.parse(line));
    expect(events.filter(event=>event.type==="send_accepted")).toHaveLength(1);
    expect(events.at(-1).type).toBe("generation_complete");
    expect(driver.runs).toBe(1);
  } finally { release(); await server.close(); }
});

test("an admitted action waits for maintenance and can be cancelled without Send", async () => {
  let entered!: () => void, release!: () => void;
  const started = new Promise<void>(resolve=>{entered=resolve;});
  const gate = new Promise<void>(resolve=>{release=resolve;});
  class MaintenanceDriver extends ImmediateDriver {
    runs = 0;
    async refreshConnector() {
      entered(); await gate;
      return {appId:"fixture",refreshObserved:true,permissionChanged:false,schemaVerified:false as const};
    }
    override async execute(execution: DriverExecution) { this.runs++; await super.execute(execution); }
  }
  const root = await mkdtemp(join(tmpdir(), "browser-maintenance-admission-"));
  const driver = new MaintenanceDriver(), coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const server = await startControlServer(coordinator, join(root, "descriptor.json"));
  const headers = {Authorization:`Bearer ${server.token}`,"Content-Type":"application/json"};
  const maintenance = coordinator.refreshConnector("Fixture"); await started;
  try {
    const response = await fetch(`${server.endpoint}/v1/actions/execute`, {
      method:"POST",headers,body:JSON.stringify(executeBody()),signal:AbortSignal.timeout(250),
    });
    expect(await response.json()).toMatchObject({state:"queued",sendAccepted:false});
    expect(driver.runs).toBe(0);
    const cancelled = coordinator.cancel({actionId:"control-action",idempotencyKey:"cancel-waiting"});
    release(); await maintenance;
    expect(await cancelled).toMatchObject({state:"failed",cancelled:true});
    const events = [];
    for await (const event of coordinator.events("control-action",0)) events.push(event);
    expect(events.at(-1)).toMatchObject({type:"failed",code:"cancelled"});
    expect(events.some(event=>event.type==="ambiguous_send"||event.type==="send_accepted")).toBeFalse();
    expect(await coordinator.execute(executeBody())).toMatchObject({state:"failed",sendAccepted:false});
    expect(driver.runs).toBe(0);
  } finally {release(); await maintenance; await server.close();}
});

test("conversation approvals require the private bearer and a strict conversation/account/connector request",async()=>{
  class ApprovalDriver extends ImmediateDriver {
    checks:ConversationApprovalRequest[]=[];
    async approvePendingConversation(request:ConversationApprovalRequest){this.checks.push(request);return true;}
  }
  const root=await mkdtemp(join(tmpdir(),"approval-control-"));
  const driver=new ApprovalDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD);
  const server=await startControlServer(coordinator,join(root,"descriptor.json"));
  try {
    const body={url:"https://chatgpt.com/c/approval",expectedAccountFingerprint:"account-test",connectorName:"Fixture connector"};
    const post=(payload:unknown,authorized=true)=>fetch(`${server.endpoint}/v1/approvals/conversation`,{
      method:"POST",headers:{"Content-Type":"application/json",...(authorized?{Authorization:`Bearer ${server.token}`}:{})},body:JSON.stringify(payload)});
    expect((await post(body,false)).status).toBe(401);
    expect((await post({...body,extra:true})).status).toBe(400);
    expect((await post({...body,url:"https://example.com/c/approval"})).status).toBe(400);
    expect((await post({...body,url:"https://chatgpt.com/settings"})).status).toBe(400);
    expect((await post({...body,url:"not-a-url"})).status).toBe(400);
    expect(driver.checks).toEqual([]);
    const response=await post(body);expect(response.status).toBe(200);
    expect(await response.json()).toEqual({protocolVersion:1,build:TEST_BUILD,approved:true});
    expect(driver.checks).toEqual([body]);
  } finally {await server.close();}
});

test("private loopback server enforces bearer auth and streams bounded NDJSON", async () => {
  const root = await mkdtemp(join(tmpdir(), "codexify-browser-control-"));
  const descriptorPath = join(root, "descriptor.json");
  const coordinator = new SingleWindowCoordinator(new ImmediateDriver(), TEST_BUILD);
  const server = await startControlServer(coordinator, descriptorPath);
  try {
    const descriptor = JSON.parse(await readFile(descriptorPath, "utf8"));
    expect(descriptor.endpoint).toBe(server.endpoint);
    expect(descriptor.token).toBe(server.token);
    expect(descriptor.kind).toBe("codexify-browser-companion");
    if (process.platform !== "win32") {
      expect((await lstat(root)).mode & 0o777).toBe(0o700);
      expect((await lstat(descriptorPath)).mode & 0o777).toBe(0o600);
    }

    expect((await fetch(`${server.endpoint}/v1/health`)).status).toBe(401);
    expect((await fetch(`${server.endpoint}/v1/liveness`)).status).toBe(401);
    const liveness=await fetch(`${server.endpoint}/v1/liveness`,{headers:{Authorization:`Bearer ${server.token}`}});
    expect(await liveness.json()).toEqual({protocolVersion:1,build:TEST_BUILD,available:true});
    const headers = { Authorization:`Bearer ${server.token}` };
    const health = await (await fetch(`${server.endpoint}/v1/health`, { headers })).json();
    expect(health).toMatchObject({ protocolVersion:1, kind:"codexify-browser-companion", authenticated:true });

    const executed = await (await fetch(`${server.endpoint}/v1/actions/execute`, {
      method:"POST", headers:{ ...headers, "Content-Type":"application/json" }, body:JSON.stringify(executeBody()),
    })).json();
    expect(executed.actionId).toBe("control-action");
    const stream = await fetch(`${server.endpoint}/v1/actions/control-action/events?after=0`, { headers });
    expect(stream.headers.get("content-type")).toContain("application/x-ndjson");
    const lines = (await stream.text()).trim().split("\n").filter(Boolean).map(line => JSON.parse(line));
    expect(lines.map(event => event.type)).toEqual([
      "queued", "navigating", "model_verified", "connector_verified", "send_accepted", "answer_delta", "generation_complete",
    ]);
    expect(lines.every((event, index) => event.sequence === index + 1)).toBeTrue();

    const status = await (await fetch(`${server.endpoint}/v1/actions/control-action`, { headers })).json();
    expect(status.state).toBe("idle");
    expect(JSON.stringify(status)).not.toContain(server.token);
  } finally {
    await server.close();
  }
  expect(await readFile(descriptorPath, "utf8").catch(() => null)).toBeNull();
});

test("request schemas are strict and descriptor symlinks fail closed", async () => {
  const root = await mkdtemp(join(tmpdir(), "codexify-browser-control-"));
  const coordinator = new SingleWindowCoordinator(new ImmediateDriver(), TEST_BUILD);
  const server = await startControlServer(coordinator, join(root, "descriptor.json"));
  try {
    const headers = { Authorization:`Bearer ${server.token}`, "Content-Type":"application/json" };
    const bad = await fetch(`${server.endpoint}/v1/actions/execute`, {
      method:"POST", headers, body:JSON.stringify({ ...executeBody(), javascript:"document.cookie" }),
    });
    expect(bad.status).toBe(400);
    expect(await bad.text()).not.toContain("document.cookie");
  } finally { await server.close(); }

  const target = join(root, "target.json");
  const linked = join(root, "linked.json");
  await Bun.write(target, "unchanged");
  await symlink(target, linked);
  await expect(startControlServer(coordinator, linked)).rejects.toThrow("symlink");
  expect(await readFile(target, "utf8")).toBe("unchanged");
  if (process.platform !== "win32") await chmod(root, 0o700);
});

test("shutdown aborts an active action before waiting for its NDJSON client", async () => {
  const root = await mkdtemp(join(tmpdir(), "codexify-browser-control-"));
  const coordinator = new SingleWindowCoordinator(new BlockingDriver(), TEST_BUILD);
  const server = await startControlServer(coordinator, join(root, "descriptor.json"));
  const headers = { Authorization:`Bearer ${server.token}` };
  await fetch(`${server.endpoint}/v1/actions/execute`, {
    method:"POST",
    headers:{ ...headers, "Content-Type":"application/json" },
    body:JSON.stringify(executeBody()),
  });
  const stream = await fetch(`${server.endpoint}/v1/actions/control-action/events?after=0`, { headers });

  await expect(Promise.race([
    server.close().then(() => "closed"),
    Bun.sleep(1_000).then(() => "timeout"),
  ])).resolves.toBe("closed");

  const events = (await stream.text()).trim().split("\n").filter(Boolean).map(line => JSON.parse(line));
  expect(events.at(-1)?.type).toBe("ambiguous_send");
});

test("control responses never expose raw browser page or request failures", async () => {
  const root = await mkdtemp(join(tmpdir(), "codexify-browser-control-"));
  const coordinator = new SingleWindowCoordinator(new PrivateFailureDriver(), TEST_BUILD);
  const server = await startControlServer(coordinator, join(root, "descriptor.json"));
  const headers = { Authorization:`Bearer ${server.token}`, "Content-Type":"application/json" };
  try {
    for (const [path, body] of [
      ["/v1/session/capabilities", {}],
      ["/v1/window/reveal", { url:"https://chatgpt.com/c/private-conversation" }],
    ] as const) {
      const response = await fetch(`${server.endpoint}${path}`, {
        method:"POST",
        headers,
        body:JSON.stringify(body),
      });
      const text = await response.text();
      expect(response.status).toBe(409);
      expect(text).toBe("Browser companion operation failed");
      expect(text).not.toContain("private-conversation");
      expect(text).not.toContain("private page text");
      expect(text).not.toContain("private browser prompt");
    }
  } finally {
    await server.close();
  }
});

test('disconnecting a control request cancels its UI work and frees the queue',async()=>{
 const root=await mkdtemp(join(tmpdir(),'codexify-browser-disconnect-'));
 let entered!:()=>void,cancelled=false;
 const ready=new Promise<void>(resolve=>{entered=resolve;});
 class DisconnectDriver extends ImmediateDriver {
  override async capabilities(signal?:AbortSignal):Promise<CapabilityCatalog>{
   entered();
   await new Promise<void>((_resolve,reject)=>signal!.addEventListener('abort',()=>{cancelled=true;reject(new DOMException('aborted','AbortError'));},{once:true}));
   return super.capabilities();
  }
 }
 const coordinator=new SingleWindowCoordinator(new DisconnectDriver(),TEST_BUILD);
 const server=await startControlServer(coordinator,join(root,'descriptor.json'));
 const headers={Authorization:`Bearer ${server.token}`,'Content-Type':'application/json'};
 try {
  const abort=new AbortController();
  const pending=fetch(`${server.endpoint}/v1/session/capabilities`,{method:'POST',headers,body:'{}',signal:abort.signal}).catch(error=>error);
  await ready;abort.abort();await pending;
  for(let attempt=0;!cancelled&&attempt<50;attempt++)await Bun.sleep(10);
  expect(cancelled).toBeTrue();
  const response=await fetch(`${server.endpoint}/v1/window/reveal`,{method:'POST',headers,body:JSON.stringify({url:null})});
  expect(response.status).toBe(200);expect((await response.json() as any).revealed).toBeTrue();
 }finally{await server.close();}
});
