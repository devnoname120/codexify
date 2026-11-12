import {withConversationCooldownBypass} from "./navigation";
import {submissionTracingEnabled} from "./submission-network";
import { createHash } from "node:crypto";
import { asCompanionError, BrowserCompanionError, ConversationCooldownError, CONVERSATION_UNAVAILABLE_MESSAGE } from "./errors";
import { modelSelectionAllowed } from "./model-fallback";
import { observeReadOnly } from "./observation";
import {
  COMPANION_KIND,
  MAX_DELTA_BYTES,
  MAX_EVENT_LINE_BYTES,
  MAX_EVENT_STREAM_BYTES,
  PROTOCOL_VERSION,
  actionEventSchema,
  actionSnapshotSchema,
  cancelActionRequestSchema,
  canonicalChatGptConversationUrl,
  executeActionRequestSchema,
  conversationApprovalRequestSchema,
  type ConversationApprovalRequest,
  renderActionPrompt,
  type ActionEvent,
  type ActionPrompt,
  type ActionSnapshot,
  type ActionState,
  type CancelActionRequest,
  type CapabilityCatalog,
  type CompanionHealth,
  type ExecuteActionRequest,
  validateProtocolBuild,
} from "./protocol";

type WithoutEventEnvelope<T> = T extends ActionEvent
  ? Omit<T, "sequence" | "actionId" | "atMs">
  : never;
// An omitted connector is a driver-only milestone, never connector verification
// on the wire. It renews the next submission phase's existing timeout budget.
const STOP_PROGRESS_STEPS = ["initial", "retry", "recovery", "confirmation", "complete"] as const;
export type StopProgressStep = typeof STOP_PROGRESS_STEPS[number];
export type DriverEvent = WithoutEventEnvelope<ActionEvent> | {type:"connector_omitted"}
  | {type:"editor_recovery";step:"started"|"document_loaded"|"ready"}
  | {type:"navigation_progress";step:"document_loaded"|"editor_ready"|"account_verified"}
  | {type:"connector_progress";step:"query_inserted"|"menu_ready"|"app_selected"}
  | {type:"submission_progress";step:"send_clicked"}
  | {type:"stop_progress";step:StopProgressStep};

export const MAX_RETAINED_ACTIONS = 8;
export const BROWSER_OPERATION_TIMEOUT_MS = 30_000;
export const PASSIVE_HEALTH_BUDGET_MS = 250;
export const PASSIVE_HEALTH_RESULT_MAX_AGE_MS = 2_000;
// Refresh confirmation can take 60 seconds after its bounded UI steps. Keep
// ownership through observation and restoration, below the client's 150s limit.
const CONNECTOR_REFRESH_TIMEOUT_MS = 120_000;
const BROWSER_CLEANUP_GRACE_MS = 5_000;
const BROWSER_HARD_STOP_MS = 2_000;
const MAX_RETIRED_ACTIONS = 1024;
const MAX_RETAINED_CANCEL_KEYS = 1024;

export interface DriverExecution {
  request: ExecuteActionRequest;
  renderedPrompt: string;
  signal: AbortSignal;
  emit(event: DriverEvent): void;
}

export type BrowserStepRunner = <T>(operation:(signal:AbortSignal)=>Promise<T>,signal?:AbortSignal)=>Promise<T>;

export interface BrowserDriver {
  presentation?(signal?:AbortSignal,screenshot?:boolean):Promise<unknown>;
  setPresentationStyle?(css:string,signal?:AbortSignal):Promise<unknown>;
  approvePendingConversation?(request:ConversationApprovalRequest,signal:AbortSignal,once?:boolean):Promise<boolean>;
  liveness?():Promise<boolean>;
  health(signal?:AbortSignal): Promise<{
    authenticated:boolean;
    accountFingerprint:string | null;
    status:"idle" | "busy" | "authentication_required" | "unavailable";
    detail:string | null;
  }>;
  login(signal?: AbortSignal): Promise<{
    authenticated:boolean;
    accountFingerprint:string | null;
    status:"idle" | "busy" | "authentication_required" | "unavailable";
    detail:string | null;
  }>;
  capabilities(signal?: AbortSignal): Promise<CapabilityCatalog>;
  prepareNewChat?(projectUrl:string | null, signal?: AbortSignal): Promise<CapabilityCatalog>;
  accountPlan?(epoch:string, signal?: AbortSignal): Promise<import("./account-settings").AccountPlan>;
  navigationAvailability?(url:string):Promise<number|null>;
  turnHandoffComplete?(url:string,account:string,handoffId:string,signal?:AbortSignal):Promise<boolean>;
  refreshConnector?(name:string, signal?: AbortSignal): Promise<{appId:string;refreshObserved:boolean;permissionChanged:boolean;schemaVerified:false}>;
  refreshConnectorSteps?(name:string,run:BrowserStepRunner,signal?:AbortSignal):Promise<{appId:string;refreshObserved:boolean;permissionChanged:boolean;schemaVerified:false}>;
  reveal(url: string | null, signal?: AbortSignal): Promise<void>;
  replacePairingDraft?(request: ExecuteActionRequest, signal?: AbortSignal): Promise<boolean>;
  recoverAuthentication?(request:ExecuteActionRequest,signal:AbortSignal):Promise<void>;
  submissionMayHaveBeenAccepted?(actionId:string): boolean;
  execute(execution: DriverExecution): Promise<void>;
  close?(): Promise<void>;
}

interface ActionRecord {
  request: ExecuteActionRequest;
  digest: string;
  snapshot: ActionSnapshot;
  events: ActionEvent[];
  eventBytes: number;
  listeners: Set<(event: ActionEvent) => void>;
  abort: AbortController;
  recoverAuthentication: boolean;
  verifiedAccountFingerprint: string | null;
  pairingContinuation: boolean;
  terminal: boolean;
  runSettledPromise: Promise<void>;
  resolveRunSettled(): void;
}

interface RetiredActionRecord {
  digest: string;
  idempotencyKey: string;
  snapshot: ActionSnapshot;
}

function requestDigest(request: ExecuteActionRequest): string {
  return createHash("sha256").update(JSON.stringify({...request,bypassConversationCooldown:request.bypassConversationCooldown??false})).digest("hex");
}

function terminalType(type: ActionEvent["type"]): boolean {
  return [
    "submission_complete", "generation_complete", "approval_required", "authentication_required", "rate_limited",
    "external_interference", "ambiguous_send", "failed", "waiting",
  ].includes(type);
}

function cloneSnapshot(snapshot: ActionSnapshot): ActionSnapshot {
  return structuredClone(snapshot);
}

function stateForEvent(type: ActionEvent["type"], current: ActionState): ActionState {
  switch (type) {
    case "queued": return "queued";
    case "waiting": return "waiting";
    case "navigating": return "navigating";
    case "composer_ready": return "navigating";
    case "model_verified": return "navigating";
    case "connector_verified": return "navigating";
    case "send_accepted":
    case "conversation_created": return "sending";
    case "status_delta":
    case "commentary_delta":
    case "answer_delta": return "generating";
    case "submission_complete":
    case "generation_complete": return "idle";
    case "approval_required": return "approval_required";
    case "authentication_required": return "authentication_required";
    case "rate_limited": return "rate_limited";
    case "external_interference": return "external_interference";
    case "ambiguous_send": return "ambiguous";
    case "failed": return "failed";
    default: return current;
  }
}

function attentionFor(event: ActionEvent): ActionSnapshot["attention"] {
  switch (event.type) {
    case "approval_required": return { code:"approval_required", message:event.message };
    case "authentication_required": return { code:"authentication_required", message:null };
    case "rate_limited": return { code:"rate_limited", message:event.message };
    case "external_interference": return { code:"external_interference", message:event.message };
    case "ambiguous_send": return { code:"ambiguous_send", message:event.message };
    case "failed": return { code:event.code, message:event.message };
    default: return null;
  }
}

function boundedEventMessage(value: string): string {
  const sanitized = value.replace(/[\u0000-\u001f\u007f]/gu, "�");
  if (!sanitized) return "Browser companion operation failed";
  const bytes = Buffer.from(sanitized, "utf8");
  const maximum = Math.min(MAX_DELTA_BYTES, Math.floor((MAX_EVENT_LINE_BYTES - 4096) / 2));
  if (bytes.length <= maximum) return sanitized;
  let end = maximum;
  while (end > 0 && (bytes[end]! & 0xc0) === 0x80) end -= 1;
  return bytes.subarray(0, end).toString("utf8") || "Browser companion operation failed";
}

const MAX_JSON_EXPANDING_CHUNK_BYTES = Math.floor((MAX_EVENT_LINE_BYTES - 4096) / 6);

function splitUtf8(value: string, maximumBytes: number): string[] {
  const chunks: string[] = [];
  let characters: string[] = [];
  let bytes = 0;
  for (const character of value) {
    const characterBytes = Buffer.byteLength(character, "utf8");
    if (characters.length > 0 && bytes + characterBytes > maximumBytes) {
      chunks.push(characters.join(""));
      characters = [];
      bytes = 0;
    }
    characters.push(character);
    bytes += characterBytes;
  }
  if (characters.length > 0) chunks.push(characters.join(""));
  return chunks;
}

function splitTextDriverEvent(event: DriverEvent): DriverEvent[] | undefined {
  switch (event.type) {
    case "status_delta":
      return splitUtf8(event.text, MAX_JSON_EXPANDING_CHUNK_BYTES).map((text, index) => ({
        ...event,
        text,
        continuation:event.continuation || index > 0,
      }));
    case "commentary_delta":
      return splitUtf8(event.markdown, MAX_JSON_EXPANDING_CHUNK_BYTES).map((markdown, index) => ({
        ...event,
        markdown,
        continuation:event.continuation || index > 0,
      }));
    case "answer_delta":
      return splitUtf8(event.markdown, MAX_JSON_EXPANDING_CHUNK_BYTES).map((markdown, index) => ({
        ...event,
        markdown,
        continuation:event.continuation || index > 0,
      }));
    default:
      return undefined;
  }
}

export class SingleWindowCoordinator {
  private readonly actions = new Map<string, ActionRecord>();
  private readonly retiredActions = new Map<string, RetiredActionRecord>();
  private readonly executeKeys = new Map<string, string>();
  private readonly cancelKeys = new Map<string, { actionId:string; receipt:{
    protocolVersion:number; build:string; actionId:string; state:ActionState; cancelled:boolean;
  } }>();
  private activeActionId: string | null = null;
  private pendingActionId: string | null = null;
  private windowOperationTail: Promise<void> = Promise.resolve();
  private retired = false;
  private driverClosed = false;
  private windowOperationActive = false;
  private observationCleanupPending = false;
  private actionCleanupPending = false;
  private connectorRefreshPending = false;
  private healthFlight?:ReturnType<BrowserDriver["health"]>;
  private healthFlightGeneration=0;
  private healthGeneration=0;
  private healthCompleted?:{pending:ReturnType<BrowserDriver["health"]>;generation:number;observedAt:number;health:Awaited<ReturnType<BrowserDriver["health"]>>};
  private closing = false;
  private pairingWindow: {actionId:string;url:string;generation:number;account:string;connector:string;timer:ReturnType<typeof setTimeout>} | null = null;

  constructor(private readonly driver: BrowserDriver, readonly build: string,
    private readonly timeouts: {operationMs:number;cleanupMs:number;hardStopMs:number;retainedCleanupMs?:number;pairingRetentionMs?:number;accountPlanMs?:number} = { operationMs:BROWSER_OPERATION_TIMEOUT_MS, cleanupMs:BROWSER_CLEANUP_GRACE_MS, hardStopMs:BROWSER_HARD_STOP_MS, accountPlanMs:45_000 }) {
    validateProtocolBuild(build);
    if (Object.values(timeouts).some(value => !Number.isFinite(value) || value <= 0)) throw new Error("Browser operation deadlines must be positive");
  }

  async liveness():Promise<{protocolVersion:number;build:string;available:boolean}> {
    const available=(this.activeActionId!==null&&!this.actionCleanupPending)||this.pendingActionId!==null||(!this.retired&&!this.driverClosed&&((this.windowOperationActive&&!this.observationCleanupPending&&!this.actionCleanupPending)||(this.driver.liveness
      ? await this.driver.liveness()
      : await this.driver.health().then(health=>health.status!=="unavailable"||health.authenticated).catch(()=>false))));
    return {protocolVersion:PROTOCOL_VERSION,build:this.build,available};
  }

  async presentation(signal?:AbortSignal,screenshot=false):Promise<unknown> {
    if(this.activeActionId||this.windowOperationActive)throw new BrowserCompanionError("Browser companion is busy; presentation inspection deferred");
    return this.withWindowOperation(async operationSignal=>{
      if(this.activeActionId)throw new BrowserCompanionError("Browser companion is busy with an active action");
      if(!this.driver.presentation)throw new Error("Presentation inspection is unavailable");
      const inspection=this.driver.presentation(operationSignal,screenshot);
      // A late read cannot change the browser. Let it settle independently so
      // a cancelled diagnostic cannot retire the window it was inspecting.
      return screenshot?inspection:observeReadOnly(inspection,operationSignal,6000);
    },signal,false,6000,"presentation screenshot");
  }
  async setPresentationStyle(css:string,signal?:AbortSignal):Promise<unknown> {
    if(this.activeActionId||this.windowOperationActive)throw new BrowserCompanionError("Browser companion is busy; fast presentation update deferred");
    return this.withWindowOperation(async operationSignal=>{
      if(this.activeActionId||!this.driver.setPresentationStyle)throw new Error("Fast presentation update is unavailable while busy");
      return this.driver.setPresentationStyle(css,operationSignal);
    },signal,false,6000,"presentation style update");
  }

  async health(): Promise<CompanionHealth> {
    return this.inspectCoordinatorHealth(true);
  }

  get busy():boolean {
    return this.retired || this.activeActionId !== null || this.pendingActionId !== null || this.windowOperationActive
      || this.actionCleanupPending || this.observationCleanupPending;
  }

  // An admitted action owns its page before its asynchronous preflight starts.
  get ownedActionId():string | null { return this.activeActionId ?? this.pendingActionId; }
  get retiredWindow():boolean {return this.retired;}

  // Action preflight can await the actual observation instead of treating the
  // passive reader's short warming response as a failed or delayed action.
  async readyHealth():Promise<CompanionHealth> {
    return this.inspectCoordinatorHealth(this.busy);
  }

  private invalidatePassiveHealth():void {
    this.healthGeneration++;this.healthCompleted=undefined;
  }

  private healthInspection(refresh:boolean):ReturnType<BrowserDriver["health"]> {
    if(!refresh&&this.healthFlight)return this.healthFlight;
    const previous=this.healthFlight,generation=this.healthGeneration;
    // Full verification waits for an old passive observation, then starts a
    // fresh read. Invalidation never starts an overlapping session probe.
    const pending=previous?previous.catch(()=>{}).then(()=>this.driver.health()):this.driver.health();
    this.healthFlight=pending;this.healthFlightGeneration=generation;
    void pending.then(health=>{
      if(generation===this.healthGeneration&&!this.activeActionId&&!this.windowOperationActive&&!this.retired&&!this.driverClosed)
        this.healthCompleted={pending,generation,observedAt:Date.now(),health};
    }).finally(()=>{if(this.healthFlight===pending)this.healthFlight=undefined;}).catch(()=>{});
    return pending;
  }

  private async inspectCoordinatorHealth(passive:boolean): Promise<CompanionHealth> {
    const owner=this.activeActionId ?? this.pendingActionId;
    const active=owner ? this.actions.get(owner) : undefined;
    if(active) {
      // The action owns the tab. Repeated health requests from queued chats
      // must not compete with navigation/editor work on Chrome's main thread.
      // This is the last verified identity, not permission for another Send;
      // the driver still verifies the actual account before submitting.
      return {protocolVersion:PROTOCOL_VERSION,kind:COMPANION_KIND,build:this.build,status:"busy",
        authenticated:active.verifiedAccountFingerprint!==null,accountFingerprint:active.verifiedAccountFingerprint,
        activeActionId:active.request.actionId};
    }
    if (this.retired) return {protocolVersion:PROTOCOL_VERSION,kind:COMPANION_KIND,build:this.build,status:"unavailable",authenticated:false,accountFingerprint:null,activeActionId:null};
    // Several worker/UI polls can arrive together while no action owns the tab.
    // A passive HTTP poll must finish before the native manager's short health
    // deadline, while the one real session inspection continues independently.
    let health:Awaited<ReturnType<BrowserDriver["health"]>>|undefined;
    // This is a short-lived observation, not a single-reader ticket. UI and
    // readiness polls must not steal the only verified result from queued work
    // and repeatedly force its preflight into a new warming/30-second wait.
    const completed=this.healthCompleted;
    if(passive&&completed&&completed.generation===this.healthGeneration&&Date.now()-completed.observedAt<=PASSIVE_HEALTH_RESULT_MAX_AGE_MS)
      health=completed.health;
    else {
      if(!passive)this.invalidatePassiveHealth();
      const pending=this.healthInspection(!passive);
      const generation=this.healthFlightGeneration;
      let timer:ReturnType<typeof setTimeout>|undefined;
      try {
        health=passive?await Promise.race([pending,new Promise<undefined>(resolve=>{timer=setTimeout(()=>resolve(undefined),PASSIVE_HEALTH_BUDGET_MS);})]):await pending;
      } finally {if(timer)clearTimeout(timer);}
      if(passive&&generation!==this.healthGeneration)health=undefined;
    }
    if(!health)return {protocolVersion:PROTOCOL_VERSION,kind:COMPANION_KIND,build:this.build,status:"busy",authenticated:false,accountFingerprint:null,activeActionId:null};
    return {
      protocolVersion:PROTOCOL_VERSION,
      kind:COMPANION_KIND,
      build:this.build,
      status:this.activeActionId || this.pendingActionId || this.windowOperationActive ? "busy" : health.status,
      authenticated:health.authenticated,
      accountFingerprint:health.authenticated ? health.accountFingerprint : null,
      activeActionId:this.activeActionId ?? this.pendingActionId,
    };
  }

  async approvePendingConversation(raw:ConversationApprovalRequest,signal?:AbortSignal,navigate=false,once=false):Promise<{protocolVersion:number;build:string;approved:boolean}> {
    const request = conversationApprovalRequestSchema.parse(raw);
    const receipt = (approved:boolean) => ({protocolVersion:PROTOCOL_VERSION,build:this.build,approved});
    if (this.activeActionId || this.windowOperationActive || !this.driver.approvePendingConversation) return receipt(false);
    const bounded = AbortSignal.any([AbortSignal.timeout(navigate?30_000:5_000),...(signal ? [signal] : [])]);
    return this.withWindowOperation(async operationSignal => {
      if (this.activeActionId) return receipt(false);
      if(navigate){
        const health=await this.driver.health(operationSignal);
        if(!health.authenticated||health.accountFingerprint!==request.expectedAccountFingerprint)
          throw new BrowserCompanionError('ChatGPT account changed before approval recovery','authentication_required');
        await this.driver.reveal(request.url,operationSignal);
      }
      return receipt(await this.driver.approvePendingConversation!(request,operationSignal,once));
    },bounded,false,this.timeouts.operationMs,"connector approval");
  }

  async login(signal?: AbortSignal): Promise<CompanionHealth> {
    await this.withWindowOperation(async operationSignal => {
      if (this.activeActionId) throw new Error(`Browser companion is busy with action ${this.activeActionId}`);
      const health = await this.driver.health(operationSignal);
      if (health.status === "busy") {
        throw new Error(health.detail || "Browser companion window is not idle");
      }
      await this.driver.login(operationSignal);
    }, signal,false,this.timeouts.operationMs,"login");
    return this.inspectCoordinatorHealth(false);
  }

  async capabilities(signal?: AbortSignal): Promise<CapabilityCatalog> {
    if(!this.connectorRefreshPending&&(this.activeActionId||this.windowOperationActive))throw new BrowserCompanionError("Browser companion is busy; catalogue inspection deferred");
    return this.withWindowOperation(async operationSignal => {
      if (this.activeActionId) throw new Error("Browser companion is busy with an active action");
      const health = await this.driver.health(operationSignal);
      if (health.status === "busy") {
        throw new Error(health.detail || "Browser companion window is not idle");
      }
      const catalog = await this.driver.capabilities(operationSignal);
      return { ...catalog, protocolVersion:PROTOCOL_VERSION, build:this.build };
    }, signal,false,this.timeouts.operationMs,"capability inspection");
  }

  async newChatCapabilities(projectUrl:string | null, signal?: AbortSignal): Promise<CapabilityCatalog> {
    const catalog=await this.withWindowOperation(async operationSignal => {
      if (this.activeActionId) throw new Error("Browser companion is busy with an active action");
      if (!this.driver.prepareNewChat) throw new Error("New chat preparation is unavailable");
      const catalog = await this.driver.prepareNewChat(projectUrl, operationSignal);
      return {...catalog, protocolVersion:PROTOCOL_VERSION, build:this.build};
    // Cold setup includes navigation, editor hydration and a full picker scan.
    // Each is bounded, but together can exceed an ordinary maintenance read.
    }, signal,false,this.timeouts.operationMs*3,"new chat preparation");
    // Older native recovery checks passive health immediately after preparing
    // a new chat. Finish real verification after the lease invalidates its old
    // proof, so that check cannot mistake normal warming for lost sign-in.
    signal?.throwIfAborted();
    const health=await this.readyHealth();
    signal?.throwIfAborted();
    if(!health.authenticated||health.accountFingerprint!==catalog.accountFingerprint
      ||health.status!=='idle'||health.activeActionId)
      throw new BrowserCompanionError('ChatGPT account or browser changed while preparing recovery','external_interference');
    return catalog;
  }

  async accountPlan(epoch:string, signal?: AbortSignal) {
    if(!this.connectorRefreshPending&&(this.activeActionId||this.windowOperationActive))throw new BrowserCompanionError("Browser companion is busy; account plan inspection deferred");
    return this.withWindowOperation(async operationSignal=>{
      if(this.activeActionId)throw new Error("Browser companion is busy with an active action");
      if(!this.driver.accountPlan)throw new Error("Account plan inspection is unavailable");
      return this.driver.accountPlan(epoch, operationSignal);
    }, signal,false,this.timeouts.accountPlanMs??this.timeouts.operationMs,"account plan inspection");
  }

  async navigationAvailability(url:string) {
    const availableAtMs=await this.driver.navigationAvailability?.(url)??null;
    return {protocolVersion:PROTOCOL_VERSION,build:this.build,availableAtMs};
  }

  async turnHandoffComplete(url:string,account:string,handoffId:string,signal?:AbortSignal) {
    if(this.activeActionId||this.windowOperationActive)return {protocolVersion:PROTOCOL_VERSION,build:this.build,complete:false};
    const complete=await this.withWindowOperation(async operationSignal=>{
      if(this.activeActionId || !this.driver.turnHandoffComplete)return false;
      return this.driver.turnHandoffComplete(url,account,handoffId,operationSignal);
    },signal,false,this.timeouts.operationMs,"turn handoff inspection");
    return {protocolVersion:PROTOCOL_VERSION,build:this.build,complete};
  }

  async refreshConnector(name:string, signal?: AbortSignal) {
    if(this.connectorRefreshPending)throw new BrowserCompanionError("Browser companion is busy with a connector refresh");
    this.connectorRefreshPending = true;
    try {
      return await this.withWindowOperation(async operationSignal=>{
        if(this.activeActionId)throw new BrowserCompanionError("Browser companion is busy with an active action");
        if(this.driver.refreshConnectorSteps)return this.driver.refreshConnectorSteps(name,
          // Normal phases inherit operationSignal. Restoration deliberately
          // supplies no parent so it can finish under its own cleanup deadline.
          (operation,parent)=>this.boundedObservationStep(operation,parent),operationSignal);
        if(!this.driver.refreshConnector)throw new Error("Connector maintenance is unavailable");
        return this.driver.refreshConnector(name,operationSignal);
      },signal,false,this.driver.refreshConnectorSteps?CONNECTOR_REFRESH_TIMEOUT_MS:this.timeouts.operationMs,"connector refresh");
    } finally {
      this.connectorRefreshPending = false;
    }
  }

  async reveal(url: string | null, signal?: AbortSignal): Promise<{ protocolVersion:number; build:string; revealed:boolean; activeActionId:string | null }> {
    return this.withWindowOperation(async operationSignal => {
      let destination = this.activeActionId ? null : url;
      if (destination) {
        const health = await this.driver.health(operationSignal);
        if (health.status === "busy") destination = null;
      }
      await this.driver.reveal(destination, operationSignal);
      return {
        protocolVersion:PROTOCOL_VERSION,
        build:this.build,
        revealed:true,
        activeActionId:this.activeActionId,
      };
    }, signal,false,this.timeouts.operationMs,"reveal");
  }

  async execute(raw: ExecuteActionRequest, signal?: AbortSignal): Promise<ActionSnapshot> {
    const request = executeActionRequestSchema.parse(raw);
    const digest = requestDigest(request);
    signal?.throwIfAborted();
    if (this.retired) throw new BrowserCompanionError("Browser operation recovery is required; restart the browser companion", "failed");
    const keyedAction = this.executeKeys.get(request.idempotencyKey);
    if (keyedAction && keyedAction !== request.actionId) {
      throw new Error("Browser action idempotency key belongs to another action");
    }
    const existing = this.actions.get(request.actionId) ?? this.retiredActions.get(request.actionId);
    if (existing) {
      if (existing.digest !== digest) throw new Error("Browser action ID belongs to a different request");
      return cloneSnapshot(existing.snapshot);
    }
    const reservation = this.pairingWindow;
    const continuing = reservation && request.pairingContinuationOf === reservation.actionId
      && request.url === reservation.url && request.expectedUrl === reservation.url
      && request.pairingGeneration === reservation.generation
      && request.expectedAccountFingerprint === reservation.account
      && request.connectorName === reservation.connector;
    if (this.pendingActionId || this.activeActionId && !continuing) throw new Error("Browser companion is busy with another action");
    if (reservation && request.pairingContinuationOf && !continuing) throw new Error("Browser companion is busy with another action");
    this.pruneActionHistory();

    // Acknowledge admission before any browser or account I/O. A slow lease or
    // preflight must remain reconcilable through GET, never look like a lost Send.
    let resolveRunSettled!: () => void;
    const runSettledPromise = new Promise<void>(resolve => { resolveRunSettled = resolve; });
    const record: ActionRecord = {
      request,
      digest,
      snapshot:{
        protocolVersion:PROTOCOL_VERSION,
        build:this.build,
        actionId:request.actionId,
        state:"queued",
        sendAccepted:false,
        canonicalUrl:null,
        selected:null,
        lastEventSequence:0,
        attention:null,
      },
      events:[],
      eventBytes:0,
      listeners:new Set(),
      abort:new AbortController(),
      recoverAuthentication:false,
      verifiedAccountFingerprint:null,
      pairingContinuation:Boolean(continuing),
      terminal:false,
      runSettledPromise,
      resolveRunSettled,
    };
    this.actions.set(request.actionId, record);
    this.executeKeys.set(request.idempotencyKey, request.actionId);
    this.pendingActionId = request.actionId;
    if (continuing) {
      // Keep maintenance operations that were already queued from navigating
      // before this continuation's preflight acquires the window operation lock.
      this.activeActionId = request.actionId;
    }
    this.emit(record, { type:"queued" });
    // Enqueue the preflight synchronously so later maintenance/shutdown cannot
    // overtake it. Its lifetime belongs to the action, not the HTTP connection.
    void this.prepareAndRun(record);
    return cloneSnapshot(record.snapshot);
  }

  private async prepareAndRun(record: ActionRecord): Promise<void> {
    const request = record.request;
    try {
      await this.withWindowOperation(async operationSignal => {
        this.pendingActionId = null;
        this.activeActionId = request.actionId;
        if(submissionTracingEnabled())console.error(`[submission-step] ${request.actionId} checking preflight health`);
        let health = await this.driver.health(operationSignal);
        if(submissionTracingEnabled())console.error(`[submission-step] ${request.actionId} preflight health ${health.status}`);
        if (health.status === "unavailable") throw new BrowserCompanionError(health.detail || "Browser companion Chrome window is unavailable");
        const recoverAuthentication = !health.authenticated && health.status === "authentication_required"
          && request.prompt.kind !== "create_chat";
        if ((!health.authenticated || health.status === "authentication_required") && !recoverAuthentication) {
          throw new BrowserCompanionError("Browser companion requires ChatGPT authentication");
        }
        if (!recoverAuthentication && health.accountFingerprint !== request.expectedAccountFingerprint) {
          throw new BrowserCompanionError("Browser companion account changed before the action could start");
        }
        if (health.status === "busy" && request.prompt.kind === "pair_verify"
          && await this.driver.replacePairingDraft?.(request, operationSignal)) {
          health = await this.driver.health(operationSignal);
        }
        if (health.status !== "idle" && request.prompt.kind !== "create_chat" && !recoverAuthentication && !record.pairingContinuation) {
          throw new BrowserCompanionError(health.detail || `Browser companion window is not idle (${health.status})`);
        }
        record.recoverAuthentication = recoverAuthentication;
        record.verifiedAccountFingerprint = health.authenticated ? health.accountFingerprint : null;
      }, record.abort.signal);
      await this.run(record);
    } catch (error) {
      if (!record.terminal) this.emitFailure(record, asCompanionError(error));
    } finally {
      if (this.pendingActionId === request.actionId) this.pendingActionId = null;
      // A retained conversation load keeps its lease until its cleanup settles.
      if (!this.actionCleanupPending && this.activeActionId === request.actionId && this.pairingWindow?.actionId !== request.actionId) this.activeActionId = record.pairingContinuation ? this.pairingWindow?.actionId ?? null : null;
      record.resolveRunSettled();
    }
  }

  refreshPairingWindow(actionId:string):boolean {
    const reservation=this.pairingWindow;
    if(!reservation||reservation.actionId!==actionId)return false;
    clearTimeout(reservation.timer);
    reservation.timer=setTimeout(()=>this.releasePairingWindow(actionId),this.timeouts.pairingRetentionMs??180_000);
    reservation.timer.unref?.();
    return true;
  }

  releasePairingWindow(actionId:string):boolean {
    const reservation = this.pairingWindow;
    if (!reservation || reservation.actionId !== actionId) return false;
    clearTimeout(reservation.timer);
    this.pairingWindow = null;
    if (this.activeActionId === actionId) this.activeActionId = null;
    return true;
  }

  async action(actionId: string): Promise<ActionSnapshot | undefined> {
    const current = this.actions.get(actionId)?.snapshot ?? this.retiredActions.get(actionId)?.snapshot;
    return current ? cloneSnapshot(current) : undefined;
  }

  async cancel(raw: CancelActionRequest): Promise<{
    protocolVersion:number; build:string; actionId:string; state:ActionState; cancelled:boolean;
  }> {
    const request = cancelActionRequestSchema.parse(raw);
    const cached = this.cancelKeys.get(request.idempotencyKey);
    if (cached) {
      if (cached.actionId !== request.actionId) throw new Error("Cancel idempotency key belongs to another action");
      return structuredClone(cached.receipt);
    }
    const record = this.actions.get(request.actionId);
    const retired = this.retiredActions.get(request.actionId);
    if (!record && !retired) throw new Error("Browser action was not found");
    if (record) {
      record.abort.abort();
      await record.runSettledPromise;
    }
    const receipt = {
      protocolVersion:PROTOCOL_VERSION,
      build:this.build,
      actionId:request.actionId,
      state:(record?.snapshot ?? retired!.snapshot).state,
      cancelled:true,
    };
    if (this.cancelKeys.size >= MAX_RETAINED_CANCEL_KEYS) {
      const oldest = this.cancelKeys.keys().next().value;
      if (oldest) this.cancelKeys.delete(oldest);
    }
    this.cancelKeys.set(request.idempotencyKey, { actionId:request.actionId, receipt });
    return structuredClone(receipt);
  }

  async *events(actionId: string, afterSequence: number, signal?: AbortSignal): AsyncGenerator<ActionEvent> {
    const record = this.actions.get(actionId);
    if (!record && this.retiredActions.has(actionId)) {
      throw new Error("Browser action event history is no longer retained");
    }
    if (!record) throw new Error("Browser action was not found");
    let cursor = afterSequence;
    for (;;) {
      for (const event of record.events) {
        if (event.sequence <= cursor) continue;
        if (terminalType(event.type)) await record.runSettledPromise;
        cursor = event.sequence;
        yield structuredClone(event);
        if (terminalType(event.type)) return;
      }
      if (record.terminal) {
        await record.runSettledPromise;
        return;
      }
      if (signal?.aborted) throw new DOMException("Browser event stream aborted", "AbortError");
      const event = await new Promise<ActionEvent>((resolve, reject) => {
        const listener = (next: ActionEvent) => {
          cleanup();
          resolve(next);
        };
        const onAbort = () => {
          cleanup();
          reject(new DOMException("Browser event stream aborted", "AbortError"));
        };
        const cleanup = () => {
          record.listeners.delete(listener);
          signal?.removeEventListener("abort", onAbort);
        };
        record.listeners.add(listener);
        signal?.addEventListener("abort", onAbort, { once:true });
      });
      if (event.sequence <= cursor) continue;
      if (terminalType(event.type)) await record.runSettledPromise;
      cursor = event.sequence;
      yield structuredClone(event);
      if (terminalType(event.type)) return;
    }
  }

  async close(): Promise<void> {
    this.closing = true;
    if (this.pairingWindow) this.releasePairingWindow(this.pairingWindow.actionId);
    await this.withWindowOperation(async () => {
      for (const record of this.actions.values()) record.abort.abort();
      await Promise.all([...this.actions.values()].map(record => record.runSettledPromise));
      if (!this.driverClosed) { this.driverClosed = true; await this.driver.close?.(); }
    }, undefined, true);
  }

  private async withWindowOperation<T>(operation: (signal:AbortSignal) => Promise<T>, signal?:AbortSignal, closing=false, timeoutMs=this.timeouts.operationMs, retainedOperation?:string): Promise<T> {
    let release!: () => void;
    const previous = this.windowOperationTail;
    this.windowOperationTail = new Promise<void>(resolve => { release = resolve; });
    let acquired=false;
    let unsettled:Promise<unknown>|undefined;
    let queuedTimer:ReturnType<typeof setTimeout>|undefined;
    let queuedAbort:(()=>void)|undefined;
    try {
      // A timed-out observation can retain the UI lease while its late read or
      // navigation settles. Bound callers waiting behind it without letting an
      // abandoned queue entry execute later or break ownership ordering.
      try {
        await Promise.race([previous,new Promise<never>((_resolve,reject)=>{
          queuedAbort=()=>reject(new BrowserCompanionError('Browser action was cancelled','cancelled'));
          if(signal?.aborted){queuedAbort();return;}
          signal?.addEventListener('abort',queuedAbort,{once:true});
          queuedTimer=setTimeout(()=>reject(new BrowserCompanionError('Browser companion is busy; a previous operation still owns the shared window')),
            timeoutMs+this.timeouts.cleanupMs+this.timeouts.hardStopMs);
        })]);
      } finally {
        if(queuedTimer)clearTimeout(queuedTimer);
        if(queuedAbort)signal?.removeEventListener('abort',queuedAbort);
      }
      acquired=true;
      if (this.retired && !closing) throw new BrowserCompanionError("Browser operation recovery is required; restart the browser companion", "failed");
      if (this.actionCleanupPending && !closing) throw new BrowserCompanionError("Browser companion is busy; a cancelled conversation load still owns the shared window");
      this.invalidatePassiveHealth();
      this.windowOperationActive=true;
      return await this.bounded(operation, signal,()=>false,timeoutMs,
        retainedOperation?pending=>{unsettled=pending;this.observationCleanupPending=true;}:undefined);
    } finally {
      if(!acquired){void previous.then(release);}
      else if(unsettled){
        this.releaseRetainedCleanup(unsettled,()=>{
          this.invalidatePassiveHealth();this.observationCleanupPending=false;this.windowOperationActive=false;release();
        },retainedOperation ?? 'maintenance');
      }else{this.invalidatePassiveHealth();this.windowOperationActive=false;release();}
    }
  }

  private releaseRetainedCleanup(pending:Promise<unknown>,release:()=>void,label:string):void {
    let finished=false,retiring=false;
    const finish=()=>{if(finished)return;finished=true;clearTimeout(timer);release();};
    // Allow slow restoration to finish, but a responsive renderer is not proof
    // that a cancelled operation will ever settle. Never retain its lease forever.
    const timer=setTimeout(()=>{
      retiring=true;this.retired=true;
      console.error(`[browser-lease] at=${new Date().toISOString()} retired ${label} after cancelled cleanup did not settle`);
      void (async()=>{
        try {
          if(!this.driverClosed){
            this.driverClosed=true;
            let stopTimer:ReturnType<typeof setTimeout>|undefined;
            try {await Promise.race([
              Promise.resolve().then(()=>this.driver.close?.()).catch(()=>{}),
              new Promise<void>(resolve=>{stopTimer=setTimeout(resolve,this.timeouts.hardStopMs);}),
            ]);} finally {if(stopTimer)clearTimeout(stopTimer);}
          }
        } finally {finish();}
      })();
    },this.timeouts.retainedCleanupMs??30_000);
    void pending.then(()=>{if(!retiring)finish();},()=>{if(!retiring)finish();});
  }

  private async boundedObservationStep<T>(operation:(signal:AbortSignal)=>Promise<T>,signal?:AbortSignal):Promise<T> {
    let unsettled:Promise<unknown>|undefined;
    try {return await this.bounded(operation,signal,()=>false,this.timeouts.operationMs,pending=>{unsettled=pending;});}
    catch(error){
      // A connector phase must settle before its finally block can restore the
      // page. The outer observation deadline bounds the HTTP caller while
      // retaining ownership over this delayed phase and its restoration.
      if(unsettled)await unsettled;
      throw error;
    }
  }

  private async bounded<T>(operation:(signal:AbortSignal,progress:()=>void)=>Promise<T>, parent?:AbortSignal,
    mayHaveSent:()=>boolean=()=>false,timeoutMs=this.timeouts.operationMs,retainObservation?:(pending:Promise<unknown>)=>void|boolean):Promise<T> {
    const abort = new AbortController();
    const cancel = () => abort.abort(new BrowserCompanionError("Browser action was cancelled", "cancelled"));
    if (parent?.aborted) cancel(); else parent?.addEventListener("abort",cancel,{once:true});
    const timer = setTimeout(() => abort.abort(new BrowserCompanionError(
      `Browser operation timed out after ${timeoutMs / 1000} seconds without progress`, "failed")), timeoutMs);
    // Retain the timer while an action owns browser cleanup, even without live handles.
    let onAbort!:()=>void;
    const interrupted = new Promise<{interrupted:true}>(resolve => {
      onAbort=()=>resolve({interrupted:true});
      if (abort.signal.aborted) onAbort(); else abort.signal.addEventListener("abort",onAbort,{once:true});
    });
    const progress = () => { if (!abort.signal.aborted) timer.refresh(); };
    const pending = Promise.resolve().then(() => { abort.signal.throwIfAborted(); return operation(abort.signal,progress); })
      .then(value=>({ok:true as const,value}),error=>({ok:false as const,error}));
    const wait = async <V>(promise:Promise<V>, ms:number):Promise<V|undefined> => {
      let timer!:ReturnType<typeof setTimeout>;
      try { return await Promise.race([promise,new Promise<undefined>(resolve=>{timer=setTimeout(()=>resolve(undefined),ms);})]); }
      finally { clearTimeout(timer); }
    };
    try {
      const result = await Promise.race([pending,interrupted]);
      if (!("interrupted" in result)) { if (result.ok) return result.value; throw result.error; }
      const submitted = mayHaveSent();
      const cleanup = await wait(pending,this.timeouts.cleanupMs);
      const retained=!cleanup&&retainObservation&&retainObservation(pending)!==false;
      if (!cleanup&&!retained) {
        // Never release this tab to a new operation while an old driver can still
        // mutate it. Retire and close an uncooperative driver; the next request
        // recovers through the existing browser-companion restart path.
        this.retired=true;
        console.error(`[browser-deadline] at=${new Date().toISOString()} retired action=${this.activeActionId??"preflight-or-maintenance"} sendMayHaveBeenAccepted=${submitted||mayHaveSent()} after an operation ignored cancellation`);
        if (!this.driverClosed) { this.driverClosed=true; await wait(Promise.resolve().then(()=>this.driver.close?.()).catch(()=>{}),this.timeouts.hardStopMs); }
      }
      const reason = asCompanionError(abort.signal.reason);
      throw new BrowserCompanionError(reason.message,reason.code,
        {sendAccepted:submitted || mayHaveSent() || Boolean(cleanup&&!cleanup.ok&&asCompanionError(cleanup.error).sendAccepted)});
    } finally {
      clearTimeout(timer); parent?.removeEventListener("abort",cancel); abort.signal.removeEventListener("abort",onAbort);
    }
  }

  private pruneActionHistory(): void {
    while (this.actions.size >= MAX_RETAINED_ACTIONS) {
      const oldest = [...this.actions].find(([, record]) => record.terminal);
      if (!oldest) throw new Error("Browser action history is full while an action is still active");
      const [actionId, record] = oldest;
      this.actions.delete(actionId);
      this.retiredActions.set(actionId, {
        digest:record.digest,
        idempotencyKey:record.request.idempotencyKey,
        snapshot:{ ...cloneSnapshot(record.snapshot), lastEventSequence:0 },
      });
    }
    while (this.retiredActions.size > MAX_RETIRED_ACTIONS) {
      const oldest = this.retiredActions.entries().next().value as [string, RetiredActionRecord] | undefined;
      if (!oldest) break;
      const [actionId, record] = oldest;
      this.retiredActions.delete(actionId);
      if (this.executeKeys.get(record.idempotencyKey) === actionId) {
        this.executeKeys.delete(record.idempotencyKey);
      }
      for (const [key, cached] of this.cancelKeys) {
        if (cached.actionId === actionId) this.cancelKeys.delete(key);
      }
    }
  }

  private async run(record: ActionRecord): Promise<void> {
    return withConversationCooldownBypass(record.request.bypassConversationCooldown === true, () => this.runWithNavigationPolicy(record));
  }

  private async runWithNavigationPolicy(record: ActionRecord): Promise<void> {
    const completedSteps = new Set<string>();
    let unsettled:Promise<unknown>|undefined;
    const loadingOnly=()=>record.request.prompt.kind!=="create_chat"
      && completedSteps.has("navigating") && !completedSteps.has("composer_ready")
      && !completedSteps.has("navigation:editor_ready") && !completedSteps.has("model_verified")
      && !completedSteps.has("connector_verified") && ![...completedSteps].some(key=>key.startsWith("stop:"));
    try {
      await this.bounded(async(signal,progress)=>{
        if(record.recoverAuthentication){
          // Recovery owns the window and produces a typed terminal failure if
          // reloading cannot restore the paired account. Passive health never navigates.
          this.emit(record,{type:"navigating"});progress();
          if(!this.driver.recoverAuthentication)throw new BrowserCompanionError("Browser companion requires ChatGPT authentication","authentication_required");
          await this.driver.recoverAuthentication(record.request,signal);
          const health=await this.driver.health(signal);
          if(!health.authenticated||health.status==="authentication_required")throw new BrowserCompanionError("Browser companion requires ChatGPT authentication","authentication_required");
          if(health.accountFingerprint!==record.request.expectedAccountFingerprint)throw new BrowserCompanionError("Browser companion account changed before the action could start","authentication_required");
          if(health.status!=="idle")throw new BrowserCompanionError(health.detail||"Browser companion window is not idle","external_interference");
          record.verifiedAccountFingerprint=health.accountFingerprint;
        }
        await this.driver.execute({
          request:record.request,
          renderedPrompt:renderActionPrompt(record.request.prompt as ActionPrompt, record.request.agentTicket),
          signal,
          emit:event => {
            this.emit(record,event);
            // Only a newly verified stage renews the stall deadline. Repeated
            // events and response output cannot keep browser ownership alive.
            const progressKey=event.type==='stop_progress'?`stop:${event.step}`:event.type==='editor_recovery'?`editor:${event.step}`:event.type==='navigation_progress'?`navigation:${event.step}`:event.type==='connector_progress'?`connector:${event.step}`:event.type;
            if (!record.terminal && !completedSteps.has(progressKey)
              && ["navigating","composer_ready","model_verified","connector_verified","connector_omitted","stop_progress","editor_recovery","navigation_progress","connector_progress","submission_progress","send_accepted","conversation_created"].includes(event.type)) {
              completedSteps.add(progressKey);
              progress();
            }
          },
        });
      },record.abort.signal,()=>this.driver.submissionMayHaveBeenAccepted?.(record.request.actionId)??false,this.timeouts.operationMs,pending=>{
        if(!loadingOnly()||record.snapshot.sendAccepted||this.driver.submissionMayHaveBeenAccepted?.(record.request.actionId))return false;
        // A late navigation/renderer read has not touched the composer or Send.
        // Keep its action lease until it settles rather than closing a live
        // browser. New mutations are refused, and liveness uses real probes.
        unsettled=pending;this.actionCleanupPending=true;
        console.error(`[browser-deadline] at=${new Date().toISOString()} retained action=${record.request.actionId} stage=conversation_load until cancelled work settles`);
      });
      if (!record.terminal) {
        this.emit(record, {
          type:"failed",
          code:"missing_terminal_event",
          message:"Browser driver ended without a terminal event",
        });
      }
    } catch (rawError) {
      if (record.terminal) return;
      const error = asCompanionError(rawError);
      if (error.code!=="conversation_limit"&&(record.snapshot.sendAccepted || error.sendAccepted)) {
        this.emit(record, { type:"ambiguous_send", message:boundedEventMessage(error.message) });
      } else {
        const stalledConversationLoad=loadingOnly()
          && error.code==="failed" && error.message.startsWith("Browser operation timed out");
        this.emitFailure(record,stalledConversationLoad
          ? new BrowserCompanionError(CONVERSATION_UNAVAILABLE_MESSAGE,"conversation_unavailable",{cause:error})
          : error);
      }
    } finally {
      record.resolveRunSettled();
      // Keep the native watchdog from replacing the process during browser
      // cleanup or before its terminal event has reached the event stream.
      // The retired driver still rejects every new window operation.
      if(this.retired)await new Promise(resolve=>setTimeout(resolve,250));
      const release=()=>{
        if (!this.closing && !unsettled && record.request.retainPairingWindow && record.request.prompt.kind === "create_chat"
          && record.snapshot.state === "idle" && record.snapshot.sendAccepted && !record.snapshot.attention
          && record.snapshot.canonicalUrl && this.activeActionId === record.request.actionId) {
          const timer = setTimeout(()=>this.releasePairingWindow(record.request.actionId),this.timeouts.pairingRetentionMs ?? 180_000);
          timer.unref?.();
          this.pairingWindow = {actionId:record.request.actionId,url:record.snapshot.canonicalUrl,
            generation:record.request.pairingGeneration,account:record.request.expectedAccountFingerprint,
            connector:record.request.connectorName,timer};
        } else if (this.activeActionId === record.request.actionId) {
          this.invalidatePassiveHealth();
          this.activeActionId = record.pairingContinuation ? this.pairingWindow?.actionId ?? null : null;
        }
      };
      if(unsettled)this.releaseRetainedCleanup(unsettled,()=>{this.actionCleanupPending=false;release();},'conversation load');
      else release();
    }
  }

  private emitFailure(record: ActionRecord, error: BrowserCompanionError): void {
    if(error instanceof ConversationCooldownError){
      this.emit(record,{type:"waiting",availableAtMs:error.availableAtMs});
      return;
    }
    const message = boundedEventMessage(error.message);
    switch (error.code) {
      case "authentication_required":
        this.emit(record, { type:"authentication_required" });
        break;
      case "approval_required":
        this.emit(record, { type:"approval_required", message });
        break;
      case "rate_limited":
        this.emit(record, { type:"rate_limited", message });
        break;
      case "external_interference":
        this.emit(record, { type:"external_interference", message });
        break;
      case "ambiguous_send":
        this.emit(record, { type:"ambiguous_send", message });
        break;
      default:
        this.emit(record, { type:"failed", code:error.code, message });
        break;
    }
  }

  private emit(record: ActionRecord, raw: DriverEvent): void {
    if(raw.type === "waiting" && record.snapshot.sendAccepted){
      throw new BrowserCompanionError("A submitted browser action cannot be deferred", "ambiguous_send", {sendAccepted:true});
    }
    if (record.terminal) return;
    if (raw.type === "stop_progress") {
      if (!STOP_PROGRESS_STEPS.includes(raw.step)) throw new BrowserCompanionError("Browser driver reported invalid Stop progress", "selector_drift");
      return;
    }
    if (raw.type === "connector_omitted") return;
    if (raw.type === "editor_recovery"||raw.type === "navigation_progress"||raw.type === "connector_progress"||raw.type==="submission_progress") return;
    if (raw.type === "model_verified"
      && !modelSelectionAllowed(record.request.model,record.request.effort,raw)) {
      throw new BrowserCompanionError(
        "Browser driver reported a different model or effort than requested",
        "failed",
      );
    }
    if (raw.type === "model_verified" && record.snapshot.sendAccepted
      && record.request.prompt.kind !== "create_chat" && record.snapshot.selected
      && (record.snapshot.selected.model !== raw.model || record.snapshot.selected.effort !== raw.effort)) {
      throw new BrowserCompanionError("Browser driver changed its verified model after Send","failed",{sendAccepted:true});
    }
    const pairing = record.request.prompt.kind === "pair_verify" || record.request.prompt.kind === "create_chat";
    if (raw.type === "connector_verified" && !pairing && record.request.model !== null && !record.snapshot.selected) {
      throw new BrowserCompanionError(
        "Browser driver verified the connector before the requested model and effort",
        "failed",
      );
    }
    if (raw.type === "send_accepted"
      && !pairing && record.request.model !== null && !record.snapshot.selected) {
      throw new BrowserCompanionError(
        "Browser driver accepted Send before completing model verification",
        "failed",
      );
    }
    if (["status_delta", "commentary_delta", "answer_delta", "submission_complete", "generation_complete"].includes(raw.type)
      && !record.snapshot.sendAccepted) {
      throw new BrowserCompanionError(
        "Browser driver produced response output without semantic Send acceptance",
        "failed",
      );
    }
    if (raw.type === "conversation_created" && (record.request.prompt.kind !== "create_chat"
      || !record.snapshot.sendAccepted || !canonicalChatGptConversationUrl(raw.canonicalUrl).startsWith("https://chatgpt.com/c/"))) {
      throw new BrowserCompanionError("Browser driver reported a created conversation without an accepted creation request", "external_interference", {sendAccepted:record.snapshot.sendAccepted});
    }
    if ((raw.type === "generation_complete" || raw.type === "submission_complete")
      && (!raw.canonicalUrl || (record.request.prompt.kind === "create_chat" ? !canonicalChatGptConversationUrl(raw.canonicalUrl).startsWith("https://chatgpt.com/c/") : canonicalChatGptConversationUrl(raw.canonicalUrl) !== canonicalChatGptConversationUrl(record.request.expectedUrl)))) {
      throw new BrowserCompanionError(
        "Browser driver completed in a different conversation than requested",
        "external_interference",
        { sendAccepted:true },
      );
    }
    if ((raw.type === "submission_complete" || raw.type === "generation_complete")
      && record.request.prompt.kind === "create_chat" && record.request.model !== null && !record.snapshot.selected) {
      throw new BrowserCompanionError("Browser driver completed new-chat pairing before restoring its requested model and effort", "failed", {sendAccepted:true});
    }
    const normalized: DriverEvent = raw.type === "failed" && raw.code!=="conversation_limit" && record.snapshot.sendAccepted
      ? { type:"ambiguous_send", message:raw.message }
      : raw;
    let event = actionEventSchema.parse({
      ...normalized,
      sequence:record.snapshot.lastEventSequence + 1,
      actionId:record.request.actionId,
      atMs:Date.now(),
    });
    if (Buffer.byteLength(`${JSON.stringify(event)}\n`) > MAX_EVENT_LINE_BYTES) {
      const split = splitTextDriverEvent(normalized);
      if (split) {
        for (const part of split) this.emit(record, part);
        return;
      }
      if (event.type !== "generation_complete" || event.finalMarkdown === null) {
        throw new BrowserCompanionError(
          "Browser companion event exceeds the protocol line limit",
          "failed",
          { sendAccepted:record.snapshot.sendAccepted },
        );
      }
      event = actionEventSchema.parse({ ...event, finalMarkdown:null });
    }
    const lineBytes = Buffer.byteLength(`${JSON.stringify(event)}\n`);
    const streamLimit = terminalType(event.type)
      ? MAX_EVENT_STREAM_BYTES
      : MAX_EVENT_STREAM_BYTES - MAX_EVENT_LINE_BYTES;
    if (record.eventBytes + lineBytes > streamLimit) {
      throw new BrowserCompanionError(
        "Browser companion event stream exceeds the protocol limit",
        "failed",
        { sendAccepted:record.snapshot.sendAccepted },
      );
    }
    record.events.push(event);
    record.eventBytes += lineBytes;
    record.snapshot.lastEventSequence = event.sequence;
    record.snapshot.state = stateForEvent(event.type, record.snapshot.state);
    // An approval discovered while opening an existing turn is a failed
    // preparation of THIS Send. Keep the approval event/attention for the
    // native supervisor, whose post-Send snapshot state requires acceptance.
    if(event.type==='approval_required'&&!record.snapshot.sendAccepted)record.snapshot.state='failed';
    record.snapshot.attention = attentionFor(event);
    if(event.type === "waiting")record.snapshot.cooldownUntilMs=event.availableAtMs;
    if (event.type === "model_verified") record.snapshot.selected = { model:event.model, effort:event.effort };
    if (event.type === "send_accepted") {
      if(record.pairingContinuation && this.pairingWindow)this.releasePairingWindow(this.pairingWindow.actionId);
      record.snapshot.sendAccepted = true;
      if(record.request.prompt.kind !== "create_chat")record.snapshot.canonicalUrl = record.request.expectedUrl;
    }
    if(event.type === "ambiguous_send" && record.pairingContinuation && this.pairingWindow)this.releasePairingWindow(this.pairingWindow.actionId);
    if (event.type === "conversation_created" || event.type === "generation_complete" || event.type === "submission_complete") record.snapshot.canonicalUrl = event.canonicalUrl;
    actionSnapshotSchema.parse(record.snapshot);
    for (const listener of [...record.listeners]) listener(event);
    if (terminalType(event.type)) {
      record.terminal = true;
    }
  }
}
