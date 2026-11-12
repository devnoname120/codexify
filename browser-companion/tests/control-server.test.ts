import { chmod, lstat, readFile, symlink } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { mkdtemp } from "node:fs/promises";
import { expect, test } from "bun:test";
import type { CapabilityCatalog, ConversationApprovalRequest, ExecuteActionRequest } from "../src/protocol";
import { SingleWindowCoordinator, type BrowserDriver, type DriverExecution } from "../src/browser-window";
import { startControlServer } from "../src/control-server";
import { protocolBuild } from "../src/protocol";
import { BrowserCompanionError } from "../src/errors";

const TEST_BUILD = protocolBuild("test-build");

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

function executeBody():ExecuteActionRequest {
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

test('explicit readiness HTTP waits for actual verified health instead of a warming response',async()=>{
  let release!:()=>void,settled=false;
  const gate=new Promise<void>(resolve=>release=resolve);
  class SlowHealthDriver extends ImmediateDriver {override async health(){await gate;return super.health();}}
  const root=await mkdtemp(join(tmpdir(),'ready-health-control-'));
  const coordinator=new SingleWindowCoordinator(new SlowHealthDriver(),TEST_BUILD);
  const server=await startControlServer(coordinator,join(root,'descriptor.json'));
  try {
    const pending=fetch(server.endpoint+'/v1/health?ready=1',{headers:{Authorization:'Bearer '+server.token}});
    void pending.then(()=>settled=true);await Bun.sleep(300);expect(settled).toBe(false);
    release();const response=await pending;expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({status:'idle',authenticated:true,accountFingerprint:'account-test'});
  }finally{release();await server.close();}
});

test("presentation diagnostics require private bearer auth and defer while an action owns the page",async()=>{
  class PresentationDriver extends BlockingDriver {
    reads=0;
    async presentation(_signal?:AbortSignal,screenshot=false){this.reads++;return {mode:"fast-v2",badgeVisible:true,screenshotRequested:screenshot};}
    async setPresentationStyle(css:string){return {applied:true,cssBytes:css.length};}
  }
  const root=await mkdtemp(join(tmpdir(),"presentation-control-"));
  const driver=new PresentationDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD);
  const server=await startControlServer(coordinator,join(root,"descriptor.json"));
  try {
    const url=server.endpoint+"/v1/diagnostics/presentation";
    const headers={Authorization:`Bearer ${server.token}`};
    expect((await fetch(url)).status).toBe(401);
    expect((await fetch(url+"?screenshot=other",{headers})).status).toBe(400);
    const reply=await fetch(url+"?screenshot=1",{headers});
    expect(await reply.json()).toEqual({mode:"fast-v2",badgeVisible:true,screenshotRequested:true});
    const update=()=>fetch(server.endpoint+'/v1/presentation/style',{method:'POST',headers:{...headers,'Content-Type':'application/json'},body:JSON.stringify({css:'#sidebar{display:none;}'})});
    expect((await update()).status).toBe(200);
    await coordinator.execute(executeBody());
    expect((await fetch(url,{headers})).status).toBe(409);
    expect((await update()).status).toBe(409);
    expect(driver.reads).toBe(1);
    await coordinator.cancel({actionId:"control-action",idempotencyKey:"presentation-stop"});
  }finally{await server.close();}
});

test("connector refresh yields a retryable status after another action takes the page",async()=>{
 class RefreshRaceDriver extends ImmediateDriver {
  async refreshConnector():Promise<never>{throw new BrowserCompanionError("Inspect connector settings: Another browser action changed pages during connector refresh","external_interference");}
 }
 const root=await mkdtemp(join(tmpdir(),"connector-refresh-race-"));
 const server=await startControlServer(new SingleWindowCoordinator(new RefreshRaceDriver(),TEST_BUILD),join(root,"descriptor.json"));
 try {
  const response=await fetch(`${server.endpoint}/v1/connectors/refresh`,{method:"POST",headers:{Authorization:`Bearer ${server.token}`,"Content-Type":"application/json"},body:JSON.stringify({name:"Codexify"})});
  expect(response.status).toBe(423);
 }finally{await server.close();}
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
    for(const path of ['/v1/approvals/conversation','/v1/approvals/recover-conversation']) {
    driver.checks.length=0;
    const body={url:"https://chatgpt.com/c/approval",expectedAccountFingerprint:"account-test",connectorName:"Fixture connector"};
    const post=(payload:unknown,authorized=true)=>fetch(`${server.endpoint}${path}`,{
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
    }
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

test("request schemas reject browser JavaScript fields", async () => {
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
});

test.skipIf(process.platform === "win32")("descriptor symlinks fail closed", async () => {
  const root = await mkdtemp(join(tmpdir(), "codexify-browser-control-"));
  const coordinator = new SingleWindowCoordinator(new ImmediateDriver(), TEST_BUILD);
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
