import {afterEach,expect,test} from "bun:test";
import {SingleWindowCoordinator,type BrowserDriver,type DriverExecution} from "../src/browser-window";
import {PooledWindowCoordinator} from "../src/browser-pool";
import {BrowserCompanionError} from "../src/errors";
import {protocolBuild,type ActionEvent,type CapabilityCatalog,type ExecuteActionRequest} from "../src/protocol";

const BUILD=protocolBuild("pool-fixture");
function deferred(){let resolve!:()=>void;const promise=new Promise<void>(done=>{resolve=done;});return {promise,resolve};}
type Gate=ReturnType<typeof deferred>;
async function abortable(gate:Gate,signal:AbortSignal){
  signal.throwIfAborted();
  let abort!:()=>void;
  try {await Promise.race([gate.promise,new Promise<never>((_,reject)=>{
    abort=()=>reject(new DOMException("Fixture action aborted","AbortError"));signal.addEventListener("abort",abort,{once:true});
  })]);}finally{signal.removeEventListener("abort",abort);}
}
const request=(actionId:string,overrides:Partial<ExecuteActionRequest>={}):ExecuteActionRequest=>({
  actionId,idempotencyKey:`key-${actionId}`,url:`https://chatgpt.com/c/${actionId}`,expectedUrl:`https://chatgpt.com/c/${actionId}`,
  pairingGeneration:1,connectorName:"Codexify",expectedAccountFingerprint:"account-1",model:"gpt-6-sol",effort:"high",
  prompt:{kind:"offline_chat",batchId:`batch-${actionId}`,messages:[{id:`message-${actionId}`,markdown:"Continue the fixture work."}]},
  autoApproveToolCalls:false,...overrides,
});

class LaneDriver implements BrowserDriver {
  account="account-1";
  authenticated=true;
  runs:string[]=[];
  executions=new Map<string,DriverExecution>();
  started=deferred();
  navigateGate?:Gate;
  completion=deferred();
  healthGate?:Gate;
  healthCalls=0;
  uncertain=false;
  ignoreAbort=false;
  maintenanceCalls:string[]=[];
  maintenanceGate?:Gate;
  maintenanceStarted=deferred();
  closeCalls=0;
  async health(){this.healthCalls++;await this.healthGate?.promise;return {
    authenticated:this.authenticated,accountFingerprint:this.authenticated?this.account:null,
    status:this.authenticated?"idle" as const:"authentication_required" as const,detail:null,
  };}
  async login(){this.maintenanceCalls.push("login");return this.health();}
  async capabilities():Promise<CapabilityCatalog>{this.maintenanceCalls.push("capabilities");return {
    protocolVersion:1,build:BUILD,accountFingerprint:this.account,catalogRevision:"fixture-catalog",
    models:[{id:"gpt-6-sol",label:"Fixture model",efforts:[{id:"high",label:"High",available:true,reason:null}]}],
  };}
  async setPresentationStyle(css:string){this.maintenanceCalls.push(css);this.maintenanceStarted.resolve();await this.maintenanceGate?.promise;return {applied:true};}
  async reveal(){this.maintenanceCalls.push("reveal");}
  async turnHandoffComplete(){this.maintenanceCalls.push("handoff");return true;}
  async approvePendingConversation(){this.maintenanceCalls.push("approval");return true;}
  async execute(execution:DriverExecution){
    this.runs.push(execution.request.actionId);this.executions.set(execution.request.actionId,execution);
    execution.emit({type:"navigating"});this.started.resolve();
    if(this.navigateGate)await abortable(this.navigateGate,execution.signal);
    execution.emit({type:"composer_ready"});
    execution.emit({type:"model_verified",model:execution.request.model!,effort:execution.request.effort!});
    execution.emit({type:"connector_verified"});
    if(this.uncertain)throw new BrowserCompanionError("Send outcome is uncertain","ambiguous_send",{sendAccepted:true});
    execution.emit({type:"send_accepted"});
    execution.emit({type:"answer_delta",markdown:`answer-${execution.request.actionId}`,continuation:false});
    if(this.ignoreAbort)await this.completion.promise;
    else await abortable(this.completion,execution.signal);
    execution.emit({type:"generation_complete",finalMarkdown:`answer-${execution.request.actionId}`,canonicalUrl:execution.request.expectedUrl});
  }
  async close(){this.closeCalls++;}
  release(){this.navigateGate?.resolve();this.completion.resolve();this.healthGate?.resolve();this.maintenanceGate?.resolve();}
}

const fixtures:Array<{pool:PooledWindowCoordinator;drivers:LaneDriver[]}>=[];
function fixture(){
  const drivers=[new LaneDriver(),new LaneDriver()];
  const coordinators=drivers.map(driver=>new SingleWindowCoordinator(driver,BUILD));
  const pool=new PooledWindowCoordinator(coordinators,BUILD);
  fixtures.push({pool,drivers});return {pool,drivers,coordinators};
}
afterEach(async()=>{for(const {pool,drivers} of fixtures.splice(0)){drivers.forEach(driver=>driver.release());await pool.close();}});
async function collect(pool:PooledWindowCoordinator,id:string){const events:ActionEvent[]=[];for await(const event of pool.events(id,0))events.push(event);return events;}

test("two fixed lanes navigate independently while one is blocked and a third action is refused",async()=>{
  const {pool,drivers}=fixture();drivers[0]!.navigateGate=deferred();
  await pool.execute(request("first"));await drivers[0]!.started.promise;
  await pool.execute(request("second"));await drivers[1]!.started.promise;
  expect(drivers.map(driver=>driver.runs)).toEqual([["first"],["second"]]);
  expect((await pool.action("first"))!.state).toBe("navigating");
  expect((await pool.action("second"))!.sendAccepted).toBeTrue();
  await expect(pool.execute(request("third"))).rejects.toThrow("busy");
  expect(await pool.health()).toMatchObject({status:"busy",actionCapacity:2,availableActionSlots:0,activeActionIds:["first","second"]});
  drivers.forEach(driver=>driver.release());
  expect((await collect(pool,"first")).at(-1)!.type).toBe("generation_complete");
  expect((await collect(pool,"second")).at(-1)!.type).toBe("generation_complete");
});

test("canonical conversation aliases cannot acquire a second lane",async()=>{
  const {pool,drivers}=fixture();
  await pool.execute(request("alias-first",{url:"https://chatgpt.com/g/project/c/shared?view=one",expectedUrl:"https://chatgpt.com/g/project/c/shared?view=one"}));
  await drivers[0]!.started.promise;
  await expect(pool.execute(request("alias-second",{url:"https://chatgpt.com/c/shared/",expectedUrl:"https://chatgpt.com/c/shared/"})))
    .rejects.toThrow("conversation already owns an action");
  expect(drivers[1]!.runs).toEqual([]);
  expect((await pool.health()).availableActionSlots).toBe(1);
});

test("conversation observations and approvals defer active aliases and retain their idle page affinity",async()=>{
  const {pool,drivers}=fixture();
  await pool.execute(request("owner",{url:"https://chatgpt.com/c/shared",expectedUrl:"https://chatgpt.com/c/shared"}));
  const alias="https://chatgpt.com/g/project/c/shared/?view=two";
  await expect(pool.turnHandoffComplete(alias,"account-1","handoff-one")).rejects.toThrow("already owns an action");
  await expect(pool.approvePendingConversation({url:alias,expectedAccountFingerprint:"account-1",connectorName:"Codexify"}))
    .rejects.toThrow("already owns an action");
  expect(drivers.flatMap(driver=>driver.maintenanceCalls)).toEqual([]);
  drivers[0]!.release();await collect(pool,"owner");
  expect((await pool.turnHandoffComplete(alias,"account-1","handoff-one")).complete).toBeTrue();
  expect((await pool.approvePendingConversation({url:alias,expectedAccountFingerprint:"account-1",connectorName:"Codexify"})).approved).toBeTrue();
  expect(drivers[0]!.maintenanceCalls).toEqual(["handoff","approval"]);
  expect(drivers[1]!.maintenanceCalls).toEqual([]);
});

test("idempotency conflicts are global and accepted receipts never resubmit on another lane",async()=>{
  const {pool,drivers}=fixture();const first=request("accepted");
  await pool.execute(first);await drivers[0]!.started.promise;
  expect((await pool.execute(first)).sendAccepted).toBeTrue();
  await expect(pool.execute(request("other",{idempotencyKey:first.idempotencyKey}))).rejects.toThrow("key belongs to another action");
  await expect(pool.execute({...first,effort:"medium"})).rejects.toThrow("different request");
  await pool.execute(request("parallel"));await drivers[1]!.started.promise;
  expect((await pool.execute(first)).sendAccepted).toBeTrue();
  drivers[0]!.release();await collect(pool,first.actionId);
  expect((await pool.execute(first)).state).toBe("idle");
  expect(drivers.map(driver=>driver.runs)).toEqual([["accepted"],["parallel"]]);
});

test("an uncertain Send is retained and replay returns evidence without a duplicate submission",async()=>{
  const {pool,drivers}=fixture();drivers[0]!.uncertain=true;const input=request("uncertain");
  await pool.execute(input);
  const events=await collect(pool,input.actionId);
  expect(events.at(-1)!.type).toBe("ambiguous_send");
  expect((await pool.execute(input)).state).toBe("ambiguous");
  await pool.execute(request("other-lane"));
  expect((await pool.execute(input)).state).toBe("ambiguous");
  expect(drivers.flatMap(driver=>driver.runs).filter(id=>id===input.actionId)).toHaveLength(1);
});

test("event streams and cancellation route only to the action owner across lanes",async()=>{
  const {pool,drivers}=fixture();
  await pool.execute(request("cancel-a"));await pool.execute(request("keep-b"));
  await Promise.all(drivers.map(driver=>driver.started.promise));
  const cancellation={actionId:"cancel-a",idempotencyKey:"cancel-global"};
  const receipt=await pool.cancel(cancellation);
  expect(receipt).toMatchObject({actionId:"cancel-a",cancelled:true,state:"ambiguous"});
  expect(await pool.cancel(cancellation)).toEqual(receipt);
  expect(drivers[0]!.executions.get("cancel-a")!.signal.aborted).toBeTrue();
  expect(drivers[1]!.executions.get("keep-b")!.signal.aborted).toBeFalse();
  await expect(pool.cancel({actionId:"keep-b",idempotencyKey:cancellation.idempotencyKey})).rejects.toThrow("key belongs to another action");
  const first=await collect(pool,"cancel-a");
  expect(first.every(event=>event.actionId==="cancel-a")).toBeTrue();
  expect(first.at(-1)!.type).toBe("ambiguous_send");
  drivers[1]!.release();const second=await collect(pool,"keep-b");
  expect(second.every(event=>event.actionId==="keep-b")).toBeTrue();
  expect(second.at(-1)!.type).toBe("generation_complete");
});

test("global maintenance waits for all idle lanes and excludes dispatch while it runs",async()=>{
  const {pool,drivers}=fixture();
  await pool.execute(request("maintenance-a"));await pool.execute(request("maintenance-b"));
  drivers[0]!.release();await collect(pool,"maintenance-a");
  await expect(pool.capabilities()).rejects.toThrow("requires idle action pages");
  await expect(pool.setPresentationStyle("body { color: red; }")).rejects.toThrow("requires idle action pages");
  expect(drivers.flatMap(driver=>driver.maintenanceCalls)).toEqual([]);
  drivers[1]!.release();await collect(pool,"maintenance-b");
  drivers[0]!.maintenanceGate=deferred();
  const update=pool.setPresentationStyle("body { color: blue; }");await drivers[0]!.maintenanceStarted.promise;
  await expect(pool.execute(request("during-style"))).rejects.toThrow("busy");
  expect((await pool.health()).availableActionSlots).toBe(0);
  drivers[0]!.maintenanceGate.resolve();await update;
  expect((await pool.capabilities()).accountFingerprint).toBe("account-1");
});

test("conflicting account observations fail closed for pooled readiness and new actions",async()=>{
  const {pool,drivers}=fixture();drivers[1]!.account="account-2";
  expect(await pool.readyHealth()).toMatchObject({status:"authentication_required",authenticated:false,accountFingerprint:null,availableActionSlots:0});
  await expect(pool.execute(request("account-conflict"))).rejects.toThrow(/account|authentication/i);
  expect(drivers.flatMap(driver=>driver.runs)).toEqual([]);
});

test("warming capacity does not claim authentication and explicit readiness waits for actual proof",async()=>{
  const {pool,drivers}=fixture();drivers.forEach(driver=>{driver.healthGate=deferred();});
  expect(await pool.health()).toMatchObject({status:"busy",authenticated:false,accountFingerprint:null,actionCapacity:2,availableActionSlots:2,activeActionIds:[]});
  let ready=false;const pending=pool.readyHealth().then(health=>{ready=true;return health;});
  await Bun.sleep(0);expect(ready).toBeFalse();
  drivers.forEach(driver=>driver.healthGate!.resolve());
  expect(await pending).toMatchObject({status:"idle",authenticated:true,accountFingerprint:"account-1",actionCapacity:2,availableActionSlots:2,activeActionIds:[]});
});

test("a retired primary page preserves healthy lane actions and idle maintenance",async()=>{
  const drivers=[new LaneDriver(),new LaneDriver()];drivers[0]!.ignoreAbort=true;
  const coordinators=[new SingleWindowCoordinator(drivers[0]!,BUILD,{operationMs:20,cleanupMs:10,hardStopMs:10}),new SingleWindowCoordinator(drivers[1]!,BUILD)];
  const pool=new PooledWindowCoordinator(coordinators,BUILD);fixtures.push({pool,drivers});
  await pool.execute(request("retire-primary"));
  expect((await collect(pool,"retire-primary")).at(-1)!.type).toBe("ambiguous_send");
  const until=Date.now()+2_000;
  while(coordinators[0]!.ownedActionId!==null&&Date.now()<until)await Bun.sleep(5);
  expect(coordinators[0]!.ownedActionId).toBeNull();
  expect(drivers[0]!.closeCalls).toBe(1);expect(drivers[1]!.closeCalls).toBe(0);
  expect((await pool.health()).availableActionSlots).toBe(1);
  await pool.setPresentationStyle("body { color: green; }");
  expect(drivers[0]!.maintenanceCalls).toEqual([]);
  expect(drivers[1]!.maintenanceCalls).toEqual(["body { color: green; }"]);
  await pool.execute(request("healthy-after-primary-retired"));
  const started=Date.now()+2_000;
  while(!drivers[1]!.runs.length&&Date.now()<started)await Bun.sleep(5);
  expect(drivers[1]!.runs).toEqual(["healthy-after-primary-retired"]);
});

test("pool shutdown closes each action page before the shared context callback exactly once",async()=>{
  const drivers=[new LaneDriver(),new LaneDriver()];let contextCloses=0;
  const pool=new PooledWindowCoordinator(drivers.map(driver=>new SingleWindowCoordinator(driver,BUILD)),BUILD,async()=>{
    expect(drivers.map(driver=>driver.closeCalls)).toEqual([1,1]);contextCloses++;
  });
  fixtures.push({pool,drivers});
  await Promise.all([pool.close(),pool.close()]);await pool.close();
  expect(contextCloses).toBe(1);
  expect(await pool.health()).toMatchObject({status:"unavailable",authenticated:false,availableActionSlots:0});
});
