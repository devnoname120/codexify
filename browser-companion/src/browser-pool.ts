import {createHash} from "node:crypto";
import {SingleWindowCoordinator} from "./browser-window";
import {BrowserCompanionError} from "./errors";
import {canonicalChatGptConversationUrl,executeActionRequestSchema,cancelActionRequestSchema,
  COMPANION_KIND,PROTOCOL_VERSION,type CompanionHealth,type ActionSnapshot,type ExecuteActionRequest} from "./protocol";

type Coordinator = SingleWindowCoordinator;
type Lane = {coordinator:Coordinator;reserved:boolean;lastUsed:number;replacement?:Promise<void>;retryReplacementAt?:number};
type OwnedAction = {lane:Lane;coordinator:Coordinator;digest:string;key:string;conversation:string;newChat:boolean};

/** Fixed pages share one private profile, while action state and DOM ownership
 * remain local to each page. Dispatch serializes only preflight/assignment;
 * navigation and accepted action execution run independently. */
export class PooledWindowCoordinator {
  private readonly lanes:Lane[];
  private readonly actions=new Map<string,OwnedAction>();
  private readonly keys=new Map<string,string>();
  private readonly cancelKeys=new Map<string,string>();
  private readonly observations=new Set<string>();
  private dispatchTail:Promise<void>=Promise.resolve();
  private maintenance=false;
  private closed=false;

  constructor(coordinators:readonly Coordinator[],readonly build:string,private readonly closeContext?:()=>Promise<void>,
    private readonly replaceLane?:()=>Promise<Coordinator>) {
    if(coordinators.length<1||coordinators.length>4)throw new Error("Browser action pages must be between one and four");
    this.lanes=coordinators.map(coordinator=>({coordinator,reserved:false,lastUsed:0}));
  }

  private free(lane:Lane):boolean {return !lane.reserved&&!lane.coordinator.busy;}
  private repairRetiredPages():void {
    if(this.closed||!this.replaceLane)return;
    for(const [index,lane] of this.lanes.entries()) {
      if(lane.reserved||lane.replacement||!lane.coordinator.retiredWindow||lane.coordinator.ownedActionId!==null
        ||Date.now()<(lane.retryReplacementAt??0))continue;
      lane.reserved=true;
      lane.replacement=(async()=>{
        try {
          // Old actions retain their original coordinator and receipts. Only
          // new work may use the replacement page after its startup completes.
          await lane.coordinator.close();
          const replacement=await this.replaceLane!();
          if(this.closed){await replacement.close();return;}
          lane.coordinator=replacement;lane.lastUsed=Date.now();lane.retryReplacementAt=undefined;
          console.error(`[browser-lane] at=${new Date().toISOString()} replaced retired page ${index+1}`);
        }catch{
          lane.retryReplacementAt=Date.now()+30_000;
          console.error(`[browser-lane] at=${new Date().toISOString()} replacement deferred for page ${index+1}`);
        }finally{lane.reserved=false;lane.replacement=undefined;}
      })();
    }
  }
  private capacity(health:CompanionHealth,available:number):CompanionHealth {
    const activeActionIds=this.lanes.map(l=>l.coordinator.ownedActionId).filter((id):id is string=>id!==null);
    return {...health,actionCapacity:this.lanes.length,availableActionSlots:available,activeActionIds,
      activeActionId:available>0?null:activeActionIds.length===1?activeActionIds[0]!:null};
  }
  private async observe(ready:boolean):Promise<CompanionHealth> {
    if(this.closed)return this.capacity({protocolVersion:PROTOCOL_VERSION,kind:COMPANION_KIND,build:this.build,
      status:"unavailable",authenticated:false,accountFingerprint:null,activeActionId:null},0);
    this.repairRetiredPages();
    const free=this.maintenance?[]:this.lanes.filter(l=>this.free(l));
    const observations=await Promise.all(this.lanes.map(l=>ready&&free.includes(l)?l.coordinator.readyHealth():l.coordinator.health()));
    // Ownership can change while the asynchronous health reads settle. Count
    // free slots and active actions from the same current ownership snapshot.
    const currentFree=this.maintenance?[]:this.lanes.filter(l=>this.free(l));
    const usable=observations.filter((h,i)=>currentFree.includes(this.lanes[i]!)&&h.status!=="unavailable"&&h.status!=="authentication_required");
    const proven=usable.filter(h=>h.authenticated&&h.status==="idle");
    const accounts=new Set(observations.filter(h=>h.authenticated).map(h=>h.accountFingerprint));
    if(accounts.size>1)return this.capacity({protocolVersion:PROTOCOL_VERSION,kind:COMPANION_KIND,build:this.build,
      status:"authentication_required",authenticated:false,accountFingerprint:null,activeActionId:null},0);
    const chosen=proven[0]??usable[0]??observations[0]!;
    // Warming is advertised as available capacity, never as authentication.
    // The explicit ready observation and each execute still prove the account.
    return this.capacity({...chosen,status:this.maintenance||currentFree.length===0?"busy":chosen.status},usable.length);
  }
  health=()=>this.observe(false);
  readyHealth=()=>this.observe(true);
  async liveness() {
    const results=await Promise.all(this.lanes.map(l=>l.coordinator.liveness()));
    return {protocolVersion:PROTOCOL_VERSION,build:this.build,available:!this.closed&&results.some(r=>r.available)};
  }

  private async exclusive<T>(operation:(coordinator:Coordinator)=>Promise<T>):Promise<T> {
    this.repairRetiredPages();
    const available=this.lanes.find(l=>this.free(l));
    if(this.closed||this.maintenance||!available||this.lanes.some(l=>l.reserved
      ||(l.coordinator.busy&&(!l.coordinator.retiredWindow||l.coordinator.ownedActionId!==null))))
      throw new BrowserCompanionError("Browser companion is busy; account maintenance requires idle action pages");
    this.maintenance=true;
    try {return await operation(available.coordinator);}finally{this.maintenance=false;}
  }
  private laneFor(url:string):Lane | undefined {
    let key:string;try {key=canonicalChatGptConversationUrl(url);}catch{return undefined;}
    const entry=[...this.actions.values()].reverse().find(a=>a.conversation===key);
    return entry?.lane;
  }
  private async refreshConversationOwners():Promise<void> {
    await Promise.all([...this.actions.entries()].filter(([,action])=>action.newChat).map(async([id,owned])=>{
      const snapshot=await owned.coordinator.action(id);
      if(snapshot?.canonicalUrl) {
        const key=canonicalChatGptConversationUrl(snapshot.canonicalUrl);
        if(key.startsWith('https://chatgpt.com/c/')){owned.conversation=key;owned.newChat=false;return;}
      }
      if(snapshot&&['idle','failed','cancelled','ambiguous'].includes(snapshot.state))owned.newChat=false;
    }));
  }
  private async onConversation<T>(url:string,operation:(coordinator:Coordinator)=>Promise<T>):Promise<T> {
    this.repairRetiredPages();
    await this.refreshConversationOwners();
    const preferred=this.laneFor(url);
    const key=canonicalChatGptConversationUrl(url);
    if(this.observations.has(key))throw new BrowserCompanionError("This ChatGPT conversation is being inspected");
    if(this.lanes.some(l=>{const id=l.coordinator.ownedActionId;return id&&this.actions.get(id)?.conversation===key;}))
      throw new BrowserCompanionError("This ChatGPT conversation already owns an action");
    const lane=preferred&&this.free(preferred)?preferred:this.lanes.find(l=>this.free(l));
    if(this.closed||this.maintenance||!lane)throw new BrowserCompanionError("Browser companion action pages are busy");
    lane.reserved=true;
    this.observations.add(key);
    try {return await operation(lane.coordinator);}finally{this.observations.delete(key);lane.reserved=false;}
  }
  login:Coordinator["login"]=signal=>this.exclusive(c=>c.login(signal));
  capabilities:Coordinator["capabilities"]=signal=>this.exclusive(c=>c.capabilities(signal));
  newChatCapabilities:Coordinator["newChatCapabilities"]=(url,signal)=>this.exclusive(c=>c.newChatCapabilities(url,signal));
  accountPlan:Coordinator["accountPlan"]=(epoch,signal)=>this.exclusive(c=>c.accountPlan(epoch,signal));
  refreshConnector:Coordinator["refreshConnector"]=(name,signal)=>this.exclusive(c=>c.refreshConnector(name,signal));
  setPresentationStyle:Coordinator["setPresentationStyle"]=(css,signal)=>this.exclusive(c=>c.setPresentationStyle(css,signal));
  presentation:Coordinator["presentation"]=(signal,screenshot)=>this.exclusive(c=>c.presentation(signal,screenshot));
  reveal:Coordinator["reveal"]=(url,signal)=>this.exclusive(c=>c.reveal(url,signal));
  turnHandoffComplete:Coordinator["turnHandoffComplete"]=(url,account,id,signal)=>this.onConversation(url,c=>c.turnHandoffComplete(url,account,id,signal));
  approvePendingConversation:Coordinator["approvePendingConversation"]=(request,signal,navigate)=>this.onConversation(request.url,c=>c.approvePendingConversation(request,signal,navigate));
  // Navigation budgets are shared by every page in the profile.
  navigationAvailability:Coordinator["navigationAvailability"]=url=>{
    const preferred=this.laneFor(url);
    return (preferred&&!preferred.coordinator.retiredWindow?preferred:this.lanes.find(l=>!l.coordinator.retiredWindow)??this.lanes[0]!).coordinator.navigationAvailability(url);
  };
  // A retained pairing window belongs to the page that created the chat.
  releasePairingWindow(actionId:string):boolean {return this.actions.get(actionId)?.coordinator.releasePairingWindow(actionId)??false;}
  refreshPairingWindow(actionId:string):boolean {return this.actions.get(actionId)?.coordinator.refreshPairingWindow(actionId)??false;}

  async execute(raw:ExecuteActionRequest,signal?:AbortSignal):Promise<ActionSnapshot> {
    const request=executeActionRequestSchema.parse(raw);
    const digest=createHash("sha256").update(JSON.stringify(request)).digest("hex");
    let release!:()=>void;
    const previous=this.dispatchTail;
    this.dispatchTail=new Promise<void>(resolve=>{release=resolve;});
    try {
      await previous;signal?.throwIfAborted();
      if(this.closed||this.maintenance)throw new BrowserCompanionError("Browser companion action pages are busy");
      this.repairRetiredPages();
      const known=this.actions.get(request.actionId);
      const keyed=this.keys.get(request.idempotencyKey);
      if(keyed&&keyed!==request.actionId)throw new Error("Browser action idempotency key belongs to another action");
      if(known) {
        if(known.digest!==digest)throw new Error("Browser action ID belongs to a different request");
        const snapshot=await known.coordinator.action(request.actionId);
        if(!snapshot)throw new Error("Browser action history expired; original outcome must not be replayed");
        if(known.coordinator.retiredWindow)return snapshot;
        return await known.coordinator.execute(request,signal);
      }
      // A fresh page preflight remains the Send authority. Conflicting known
      // page identities must nevertheless fail closed before choosing a page.
      await this.refreshConversationOwners();
      const observations=await Promise.all(this.lanes.map(l=>l.coordinator.health()));
      if(observations.some(h=>h.authenticated&&h.accountFingerprint!==request.expectedAccountFingerprint))
        throw new BrowserCompanionError("Browser companion account changed before the action could start","authentication_required");
      const newChat=request.prompt.kind==='create_chat';
      const conversation=newChat?`new-chat:${request.actionId}`:canonicalChatGptConversationUrl(request.expectedUrl);
      if(this.observations.has(conversation))throw new BrowserCompanionError("This ChatGPT conversation is being inspected");
      // A pairing continuation reuses the page retained by its new-chat action;
      // that page validates the reservation before admitting the continuation.
      const parent=request.pairingContinuationOf?this.actions.get(request.pairingContinuationOf):undefined;
      if(this.lanes.some(l=>{
        const active=l.coordinator.ownedActionId;
        return active&&active!==request.pairingContinuationOf&&this.actions.get(active)?.conversation===conversation;
      }))throw new BrowserCompanionError("This ChatGPT conversation already owns an action");
      const affinity=this.laneFor(request.expectedUrl);
      const lane=parent&&parent.coordinator===parent.lane.coordinator&&!parent.lane.reserved&&parent.coordinator.ownedActionId===request.pairingContinuationOf?parent.lane
        :affinity&&this.free(affinity)?affinity:[...this.lanes].sort((a,b)=>a.lastUsed-b.lastUsed).find(l=>this.free(l));
      if(!lane)throw new BrowserCompanionError("All browser companion action pages are busy");
      if(this.actions.size>=this.lanes.length*1100) {
        for(const [id,owned] of this.actions) {
          if(owned.coordinator.ownedActionId!==id&&!await owned.coordinator.action(id)) {
            this.actions.delete(id);if(this.keys.get(owned.key)===id)this.keys.delete(owned.key);
          }
        }
        if(this.actions.size>=this.lanes.length*1100)throw new Error("Browser action history is full");
      }
      lane.reserved=true;lane.lastUsed=Date.now();
      // Persist pool ownership before any asynchronous page preflight. Failed
      // preflight can be retried only if the single-page coordinator recorded it.
      const owned={lane,coordinator:lane.coordinator,digest,key:request.idempotencyKey,conversation,newChat};
      this.actions.set(request.actionId,owned);this.keys.set(request.idempotencyKey,request.actionId);
      try {return await lane.coordinator.execute(request,signal);}
      catch(error) {
        if(!await lane.coordinator.action(request.actionId)) {
          this.actions.delete(request.actionId);this.keys.delete(request.idempotencyKey);
        }
        throw error;
      } finally {lane.reserved=false;}
    } finally {release();}
  }
  private async owner(actionId:string):Promise<Coordinator | undefined> {
    const known=this.actions.get(actionId);if(known)return known.coordinator;
    for(const lane of this.lanes)if(await lane.coordinator.action(actionId))return lane.coordinator;
    return undefined;
  }
  async action(actionId:string) {return (await this.owner(actionId))?.action(actionId);}
  async cancel(raw:Parameters<Coordinator["cancel"]>[0]) {
    const request=cancelActionRequestSchema.parse(raw),known=this.cancelKeys.get(request.idempotencyKey);
    if(known&&known!==request.actionId)throw new Error("Browser cancel idempotency key belongs to another action");
    const coordinator=await this.owner(request.actionId);if(!coordinator)throw new Error("Browser action was not found");
    const receipt=await coordinator.cancel(request);
    this.cancelKeys.set(request.idempotencyKey,request.actionId);
    while(this.cancelKeys.size>this.lanes.length*1024)this.cancelKeys.delete(this.cancelKeys.keys().next().value!);
    return receipt;
  }
  async *events(...args:Parameters<Coordinator["events"]>) {
    const coordinator=await this.owner(args[0]);if(!coordinator)throw new Error("Browser action was not found");
    yield* coordinator.events(...args);
  }
  async close():Promise<void> {
    if(this.closed)return;this.closed=true;
    await this.dispatchTail;
    await Promise.all(this.lanes.map(l=>l.replacement));
    // Child pages settle first; only the primary closes the shared profile.
    try {
      await Promise.all(this.lanes.slice(1).map(l=>l.coordinator.close()));
      await this.lanes[0]!.coordinator.close();
    } finally {await this.closeContext?.();}
  }
}
