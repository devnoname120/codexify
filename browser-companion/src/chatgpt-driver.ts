import {observeReadOnly,RendererProbeGate} from "./observation";
import type { Frame, Locator, Page } from "patchright";
import {completedHandoff,createCompletedHandoffVerifier} from "./turn-renewal";
import {classifyPlan,identifyConnectorApp,observedConnectorApp,inspectConnectorSettings,setConnectorAllowAll,verifyConnectorAllowAll,beginConnectorToolRefresh,type ConnectorRefreshObservation,type AccountPlan} from "./account-settings";
import {ConnectorAppCache} from "./connector-cache";
import {DraftRecoveryJournal} from './draft-recovery';
import {approveConversationCard,throwIfConversationApproval} from "./conversation-approval";
import {openNewChat, createdChatUrl, assertNewChatDraft} from "./new-chat";
import { HealthAccountCache, readChatGptAccount, type ChatGptAccount } from "./account";
import { throwIfSessionAttention, throwIfConversationLimit, toolApprovalVisible } from "./browser-session";
import {navigatePage,recordConversationAvailable,acquireSubmissionSlot,conversationLoadAvailableAt} from "./navigation";
import {waitForConversationComposer} from "./conversation-readiness";
import {refreshConversationEditor} from "./editor-recovery";
import {inspectPresentation} from "./presentation";
import {observeSubmissionNetwork,submissionTracingEnabled} from "./submission-network";
import {stopControls,END_TURN_CONTROL_NAME,traceStopControls} from "./stop-controls";
import type { BrowserDriver, DriverExecution, BrowserStepRunner, StopProgressStep } from "./browser-window";
import { activeComposer, composerText, ensureEmptyOwnedComposer, insertExactPrompt, isPairingVerificationDraft, isAutomationGoalDraft, uniqueSendButton } from "./composer";
import { clearComposerState, connectorIsSelected, ConnectorMenuUnavailableError, SELECTED_CONNECTOR_SELECTOR, selectConnector, type SelectedConnector } from "./connector-selection";
import { asCompanionError, BrowserCompanionError,ModelOptionUnavailableError, CONVERSATION_UNAVAILABLE_MESSAGE, ConversationCooldownError, browserStage } from "./errors";
import { uiLabelPattern } from "./ui-localization";
import { assertSelectedModelAndEffort, selectAvailableModelAndEffort } from "./model-selection";
import { selectPairingMedium, type PairingMediumSelection } from "./pairing-model";
import { buildCapabilityCatalog, type AccountCapabilities } from "./models";
import { canonicalChatGptConversationUrl, validateChatGptConversationUrl, type CapabilityCatalog, type ExecuteActionRequest, type ConversationApprovalRequest } from "./protocol";
import {
  CHATGPT_STOP_BUTTON_SELECTOR,
  CHATGPT_COMPOSER_SELECTOR,
  assertAuthenticatedChatGptPage,
  detectChatGptAccountCapabilities,
} from "./session";
import {
  captureSubmissionBaseline,
  waitForSubmissionAcceptance,
} from "./turn-binding";

export interface SingleBrowserSurface {
  setPresentationStyle?(css:string,signal?:AbortSignal):Promise<unknown>;
  readonly requireSubmissionNetworkProof?:boolean;
  page(): Promise<Page>;
  navigate(url: string, signal?: AbortSignal): Promise<Page>;
  reveal(): Promise<void>;
  beginAction(): Promise<void>;
  endAction(): Promise<void>;
  close(): Promise<void>;
  closeActionPage?():Promise<void>;
}


export function responseCompletionReady(input: {
  responsePresent:boolean;
  generationRunning:boolean;
  answer:string;
  completionActionVisible:boolean;
}): boolean {
  return input.responsePresent
    && !input.generationRunning
    && input.answer.length > 0
    && input.completionActionVisible;
}

async function generationRunning(page: Page,signal?:AbortSignal): Promise<boolean> {
  return observeReadOnly(stopControls(page).count(),signal)
    .then(count => count > 0)
    .catch(error=>{throw new BrowserCompanionError("ChatGPT's turn state could not be verified before continuing","editor_unavailable",{cause:error});});
}

async function clearExistingDraft(page:Page,signal?:AbortSignal,expectedText?:string):Promise<Locator> {
  const composer=await activeComposer(page,30_000,signal);
  if(expectedText!==undefined && await composerText(composer)!==expectedText)
    throw new BrowserCompanionError('ChatGPT draft was edited during automatic recovery','external_interference');
  const hasDraft=await composer.evaluate(element=>{
    if(element instanceof HTMLTextAreaElement||element instanceof HTMLInputElement)return element.value.length>0;
    const clone=element.cloneNode(true) as HTMLElement;
    for(const chip of clone.querySelectorAll('[contenteditable="false"],[app-mention-path],[data-id^="plugin:"]'))chip.remove();
    return (clone.textContent??'').length>0;
  });
  if(!hasDraft)return composer;
  signal?.throwIfAborted();
  await clearComposerState(page,"",signal);
  return activeComposer(page,30_000,signal);
}

async function visibleStopDialogs(page:Page):Promise<Locator[]> {
  const dialogs:Locator[]=[];
  const frames=new Map<Frame,boolean>();
  const visibleChatGptFrame=async(frame:Frame):Promise<boolean>=>{
    if(frame===page.mainFrame())return true;
    if(frames.has(frame))return frames.get(frame)!;
    frames.set(frame,false);
    const parent=frame.parentFrame();
    if(frame.isDetached()||!parent||!await visibleChatGptFrame(parent))return false;
    if(!frame.url())return false;
    const frameUrl=new URL(frame.url());
    if(frameUrl.origin!=="https://chatgpt.com"&&frameUrl.href!=="about:blank"&&frameUrl.href!=="about:srcdoc")return false;
    try {
      // Blank/srcdoc frames must inherit ChatGPT's origin. A sandboxed app's
      // copy of the same markup must never become an approval target.
      if(!await frame.evaluate(()=>{try{return !!window.parent.document;}catch{return false;}}))return false;
      const element=await frame.frameElement();
      try {frames.set(frame,await element.isVisible());}
      finally {await element.dispose();}
      return frames.get(frame)!;
    } catch(error) {
      if(frame.isDetached())return false;
      throw error;
    }
  };
  for(const frame of page.frames()) {
    try {
      if(await visibleChatGptFrame(frame)) {
        dialogs.push(...await frame.locator('[role="dialog"], [role="alertdialog"]').filter({visible:true}).all());
      }
    } catch(error) {
      if(!frame.isDetached())throw error;
    }
  }
  return dialogs;
}

export async function stopLingeringResponse(
  page: Page,
  signal: AbortSignal,
  expected: { canonicalUrl:string; accountFingerprint:string },
  emit: DriverExecution["emit"],
  options:{mayReload?:boolean;budgetMs?:number}={},
): Promise<void> {
  if (signal.aborted) throw new DOMException("Browser action aborted", "AbortError");
  const assertTarget = async () => {
    signal.throwIfAborted();
    await observeReadOnly(throwIfSessionAttention(page),signal);
    await observeReadOnly(throwIfConversationApproval(page),signal);
    await observeReadOnly(assertAuthenticatedChatGptPage(page),signal);
    if (canonicalChatGptConversationUrl(page.url()) !== expected.canonicalUrl) {
      throw new BrowserCompanionError("ChatGPT changed conversations while stopping the previous response", "conversation_unavailable");
    }
    if ((await observeReadOnly(readChatGptAccount(page),signal)).fingerprint !== expected.accountFingerprint) {
      throw new BrowserCompanionError("ChatGPT account changed while stopping the previous response", "authentication_required");
    }
    signal.throwIfAborted();
  };
  await ensureEmptyOwnedComposer(await activeComposer(page,1_500,signal));
  const idleSettleMs=1_500;
  const initiallyIdleAt=Date.now();
  // A hydrated composer can precede Work's End turn control by over a second.
  // Require a quiet interval, rather than one missing-control observation.
  while (!await generationRunning(page,signal) && !(await observeReadOnly(visibleStopDialogs(page),signal)).length) {
    if(Date.now()-initiallyIdleAt>=idleSettleMs){await assertTarget();return;}
    await new Promise(resolve=>setTimeout(resolve,100));
    signal.throwIfAborted();
  }
  const settleMs = 20_000;
  const confirmationSettleMs=20_000;
  // Three bounded Stop attempts, one optional confirmation, and recovery work.
  const recoveryMs = options.budgetMs??90_000;
  const deadline = Date.now() + recoveryMs;
  const boundedSignal = AbortSignal.any([signal, AbortSignal.timeout(recoveryMs)]);
  const timeout = (limit:number) => Math.max(1, Math.min(limit, deadline - Date.now()));
  let lastStopRequestedAt = 0;
  const requested = async (step:Exclude<StopProgressStep,"complete">) => {
    lastStopRequestedAt = Date.now();
    emit({type:"stop_progress",step});
    // A verified editable composer is real pre-Send progress. Keep the existing
    // event stream alive during long Stop waits without inventing response output.
    try {
      await activeComposer(page,timeout(1_000),boundedSignal);
    } catch {
      boundedSignal.throwIfAborted();
      return;
    }
    emit({type:"composer_ready"});
  };
  let confirmationSubmitted = false;
  const waitForStopped = async () => {
    let settleDeadline = Math.min(deadline,Math.max(lastStopRequestedAt,Date.now())+settleMs);
    let absentSince:number|undefined;
    while (!boundedSignal.aborted) {
      const dialogs = await observeReadOnly(visibleStopDialogs(page),signal);
      const count = dialogs.length;
      if (count) {
        const dialog = dialogs[0]!;
        // A previous action or the user may already have opened this dialog.
        // Its generic shell also serves other actions: require the exact shape
        // and a localized Stop submit button before confirming it.
        if (count !== 1 || !await observeReadOnly(dialog.evaluate(element => {
          if (!element.matches('.codex-dialog[role="dialog"][aria-modal="true"][data-state="open"]')) return false;
          const forms = element.querySelectorAll('form');
          if (forms.length !== 1) return false;
          const form = forms[0]!;
          const heading = element.ownerDocument.getElementById(element.getAttribute('aria-labelledby') ?? '');
          const description = element.ownerDocument.getElementById(element.getAttribute('aria-describedby') ?? '');
          const submits = form.querySelectorAll('button[type="submit"]');
          if (!heading?.matches('h2') || !form.contains(heading) || !description || !form.contains(description)
            || !form.querySelector('.heading-dialog') || !form.querySelector('.text-codex-description')
            || submits.length !== 1 || !submits[0]!.matches('.bg-primary-solid.text-primary-solid')) return false;
          const actions = submits[0]!.parentElement!;
          return actions.querySelectorAll('button').length === 2 && actions.querySelectorAll('button[type="button"]').length === 1;
        }),boundedSignal)) {
          throw new BrowserCompanionError("ChatGPT has an unexpected dialog while stopping the response", "external_interference");
        }
        const confirm = dialog.locator('form button[type="submit"].bg-primary-solid')
          .and(dialog.getByRole('button',{name:uiLabelPattern('stopControl')})
            .or(dialog.getByRole('button',{name:END_TURN_CONTROL_NAME})));
        if (await observeReadOnly(confirm.count(),boundedSignal) !== 1) {
          throw new BrowserCompanionError("ChatGPT has an unexpected dialog while stopping the response", "external_interference");
        }
        if (!confirmationSubmitted) {
          await assertTarget();
          boundedSignal.throwIfAborted();
          await confirm.click({timeout:timeout(750),signal:boundedSignal});
          confirmationSubmitted = true;
          await requested("confirmation");
          // Confirmation is a fresh Stop request. Its full settling window starts
          // here, even when the dialog appeared near the first click's deadline.
          settleDeadline = Math.min(deadline,lastStopRequestedAt+confirmationSettleMs);
        }
      }
      // Confirmation can remove its entire iframe. Re-scan on the next poll
      // instead of waiting on a detached locator and retrying Stop prematurely.
      if (!count && !await generationRunning(page,signal)) {
        absentSince??=Date.now();
        if(Date.now()-absentSince>=idleSettleMs)return;
      } else absentSince=undefined;
      if (Date.now() >= settleDeadline) throw new Error("ChatGPT Stop has not completed");
      await new Promise(resolve => setTimeout(resolve,50));
    }
    boundedSignal.throwIfAborted();
  };
  let lastError:unknown;
  for (let attempt = 0; attempt < 3 && !boundedSignal.aborted; attempt++) {
    await assertTarget();
    if (boundedSignal.aborted) break;
    const stop = stopControls(page);
    const count = await observeReadOnly(stop.count(),boundedSignal);
    const dialogCount = (await observeReadOnly(visibleStopDialogs(page),signal)).length;
    if (count > 1 && dialogCount === 0) {
      throw new BrowserCompanionError("ChatGPT does not have one visible Stop control", "selector_drift");
    }
    try {
      if (dialogCount === 0 && count === 1) {
        // Stop can mount disabled while ChatGPT reconciles a steered turn.
        // Let the UI enable it; skipping it here waits without ever requesting Stop.
        await stop.click({ timeout:timeout(8_000), signal:boundedSignal });
        await requested((["initial","retry","recovery"] as const)[attempt]!);
      }
      await waitForStopped();
      await assertTarget();
      await ensureEmptyOwnedComposer(await activeComposer(page,timeout(1_000),boundedSignal));
      emit({type:"stop_progress",step:"complete"});
      return;
    } catch (error) {
      if (signal.aborted || error instanceof BrowserCompanionError) throw error;
      lastError = error;
    }
    if (confirmationSubmitted && (await observeReadOnly(visibleStopDialogs(page),signal)).length) break;
    if (attempt === 1 && !boundedSignal.aborted && options.mayReload!==false) {
      if ((await observeReadOnly(visibleStopDialogs(page),signal)).length) break;
      // Rehydrate a stuck Stop control only before inserting our prompt.
      await assertTarget();
      let composer:Locator;
      try {
        composer = await activeComposer(page, timeout(1_000), boundedSignal);
      } catch (error) {
        if (signal.aborted) throw error;
        lastError = error;
        break;
      }
      await ensureEmptyOwnedComposer(composer);
      try {
        emit({type:"stop_progress",step:"recovery"});
        await navigatePage(page,page.url(),{reload:true,timeout:timeout(10_000),signal:boundedSignal});
        await waitForConversationComposer(page,boundedSignal,timeout(8_000));
      } catch (error) {
        if (signal.aborted) throw error;
        lastError = error;
        break;
      }
    }
  }
  if (signal.aborted) throw new DOMException("Browser action aborted", "AbortError");
  throw new BrowserCompanionError(
    "ChatGPT did not stop the previous response; the new message was not sent",
    "selector_drift",
    { cause:lastError },
  );
}

async function exactConversationPage(
  surface: SingleBrowserSurface,
  requestedUrl: string,
  expectedUrl: string,
  signal?: AbortSignal,
  progress?:(step:"document_loaded"|"editor_ready")=>void,
): Promise<{ page:Page; canonicalUrl:string }> {
  validateChatGptConversationUrl(requestedUrl);
  validateChatGptConversationUrl(expectedUrl);
  let page = await surface.page();
  let sameConversation=false;
  try {sameConversation=canonicalChatGptConversationUrl(page.url())===canonicalChatGptConversationUrl(expectedUrl);}catch{}
  if(!sameConversation)page=await surface.navigate(requestedUrl,signal);
  else {
    let recoveryNeeded=false;
    try {await throwIfSessionAttention(page);}
    catch(error){
      if(!(error instanceof BrowserCompanionError
        &&(error.code==="conversation_unavailable"||error.code==="authentication_required")))throw error;
      recoveryNeeded=true;
    }
    // Give an existing page a short hydration window before deciding it needs
    // a recovery load. Reloading a healthy shell resets its loading work.
    if(!recoveryNeeded){
      try {await waitForConversationComposer(page,signal,3_000);}
      catch(error){
        if(!(error instanceof BrowserCompanionError && ["editor_unavailable","authentication_required","conversation_unavailable"].includes(error.code)))throw error;
        recoveryNeeded=true;
      }
    }
    if(recoveryNeeded){
      // A known load or sign-in failure needs one deliberate recovery load.
      await navigatePage(page,requestedUrl,{signal,reload:page.url()===requestedUrl});
    }
  }
  progress?.("document_loaded");
  await waitForConversationComposer(page,signal);
  progress?.("editor_ready");
  const canonicalUrl = canonicalChatGptConversationUrl(page.url());
  if (canonicalUrl !== canonicalChatGptConversationUrl(expectedUrl)) {
    throw new BrowserCompanionError(
      "ChatGPT navigated to a different conversation than the explicitly paired URL",
      "conversation_unavailable",
    );
  }
  return { page, canonicalUrl };
}

export class ChatGptBrowserDriver implements BrowserDriver {
  private readonly healthAccount = new HealthAccountCache();
  private readonly handoffProbes = new Map<string,number>();
  private maintenance?: {page:Page;account:ChatGptAccount};
  private pendingRefresh?: {page:Page;url:string;account:ChatGptAccount};
  private pendingSubmission?: {actionId:string;mayHaveSent:boolean};
  private planCache?: {epoch:string;planType:string;plan:AccountPlan};
  private capabilitiesCache?: {
    accountFingerprint:string;
    planType:string;
    observedAtMs:number;
    capabilities:AccountCapabilities;
    catalog:CapabilityCatalog;
  };

  private readonly connectorApps:ConnectorAppCache;
  private readonly draftRecovery:DraftRecoveryJournal;
  constructor(private readonly surface: SingleBrowserSurface, private readonly build: string,connectorCachePath?:string,
    private readonly recoveryEnhancements=process.env.CODEXIFY_BROWSER_RECOVERY_ENHANCEMENTS==="1",
    shared?:{connectorApps:ConnectorAppCache;draftRecovery:DraftRecoveryJournal}) {
    this.connectorApps=shared?.connectorApps??new ConnectorAppCache(connectorCachePath);
    this.draftRecovery=shared?.draftRecovery??new DraftRecoveryJournal(this.recoveryEnhancements&&connectorCachePath?`${connectorCachePath}.drafts.json`:undefined);
  }

  fork(surface:SingleBrowserSurface):ChatGptBrowserDriver {
    return new ChatGptBrowserDriver(surface,this.build,undefined,this.recoveryEnhancements,
      {connectorApps:this.connectorApps,draftRecovery:this.draftRecovery});
  }

  private readonly rendererProbes=new RendererProbeGate();
  private readonly preparationTimings:Array<{actionId:string;totalMs:number;steps:Array<{name:string;atMs:number}>}>=[];
  private preparationTrace(actionId:string):(name:string)=>void {
    const started=performance.now();
    const timing={actionId,totalMs:0,steps:[] as Array<{name:string;atMs:number}>};
    this.preparationTimings.push(timing);
    if(this.preparationTimings.length>8)this.preparationTimings.shift();
    return name=>{timing.totalMs=Math.round(performance.now()-started);timing.steps.push({name,atMs:timing.totalMs});};
  }
  async presentation(signal?:AbortSignal,screenshot=false):Promise<unknown> {
    const page=await this.surface.page();
    return {...await inspectPresentation(page,signal,screenshot),preparations:this.preparationTimings.slice(-8)};
  }
  async setPresentationStyle(css:string,signal?:AbortSignal):Promise<unknown> {
    if(!this.surface.setPresentationStyle)throw new Error("Live fast presentation is unavailable");
    return this.surface.setPresentationStyle(css,signal);
  }
  private healthInspection?:ReturnType<ChatGptBrowserDriver["inspectHealth"]>;

  async liveness():Promise<boolean> {
    try {
      const page=await this.surface.page();
      if(page.isClosed())return false;
      const responsive=await observeReadOnly(page.evaluate(()=>true),undefined,1500).catch(()=>false);
      return this.rendererProbes.record(responsive);
    }catch{return false;}
  }

  async approvePendingConversation(request:ConversationApprovalRequest,signal:AbortSignal,once=false):Promise<boolean> {
    const page = await this.surface.page();
    await this.surface.beginAction();
    try {
      return await approveConversationCard(page,request,this.connectorApps.get(request.expectedAccountFingerprint,request.connectorName),signal,once?'once':'conversation');
    } finally { await this.surface.endAction(); }
  }

  async health(signal?:AbortSignal):Promise<Awaited<ReturnType<ChatGptBrowserDriver["inspectHealth"]>>> {
    const pending=this.healthInspection??=this.inspectHealth();
    void pending.finally(()=>{if(this.healthInspection===pending)this.healthInspection=undefined;}).catch(()=>{});
    try {return await observeReadOnly(pending,signal);}
    catch(error){
      if(signal?.aborted)throw error;
      const failure=asCompanionError(error);
      return {authenticated:false,accountFingerprint:null,status:failure.code==='editor_unavailable'?"busy":"unavailable",detail:failure.message};
    }
  }

  private async inspectHealth(): Promise<{
    authenticated:boolean;
    accountFingerprint:string | null;
    status:"idle" | "busy" | "authentication_required" | "unavailable";
    detail:string | null;
  }> {
    const signal=AbortSignal.timeout(5_000);
    const read=<T>(operation:()=>Promise<T>):Promise<T>=>{
      signal.throwIfAborted();return observeReadOnly(operation(),signal);
    };
    try {
      const page = await read(()=>this.surface.page());
      if(page.isClosed())throw new BrowserCompanionError("The shared Chrome ChatGPT window was closed","external_interference");
      if (this.maintenance?.page === page) {
        return {authenticated:true,accountFingerprint:this.maintenance.account.fingerprint,
          status:"busy",detail:"The shared ChatGPT tab is performing account maintenance"};
      }
      if(this.pendingRefresh?.page===page&&page.url()===this.pendingRefresh.url) {
        return {authenticated:true,accountFingerprint:this.pendingRefresh.account.fingerprint,status:"idle",detail:null};
      }
      if (new URL(page.url()).pathname.startsWith("/auth/")) {
        throw new BrowserCompanionError("Sign in to ChatGPT in the browser companion", "authentication_required");
      }
      let account:ChatGptAccount;
      try {
        account = await read(()=>this.healthAccount.read(page));
      } catch (error) {
        if (page.isClosed()) throw new BrowserCompanionError("The shared Chrome ChatGPT window was closed", "external_interference", {cause:error});
        // Missing/hydrating editors do not prove sign-out. Only explicit
        // authentication UI may turn a failed session observation into that
        // state; a slow API read must preserve the live browser and account.
        await throwIfSessionAttention(page,signal);
        throw error;
      }
      try {
        await throwIfSessionAttention(page,signal);
      } catch (error) {
        if (error instanceof BrowserCompanionError && ["conversation_unavailable","conversation_limit","conversation_load_rate_limited","rate_limited"].includes(error.code)) {
          // The account was verified independently. Keep it signed in; the
          // shared navigation budget enforces conversation and server cooldowns.
          return {authenticated:true, accountFingerprint:account.fingerprint, status:"idle", detail:error.message};
        }
        if (error instanceof BrowserCompanionError && error.code === "authentication_required") {
          this.healthAccount.invalidate();
          return {
            authenticated:false,
            accountFingerprint:null,
            status:"authentication_required",
            detail:error.message,
          };
        }
        return {
          authenticated:true,
          accountFingerprint:account.fingerprint,
          status:"unavailable",
          detail:asCompanionError(error).message,
        };
      }
      // Health checks the browser/session, not the current chat's editor.
      // Every action waits for and validates its actual target composer after
      // navigation. Repeating that here adds RPCs and false first-attempt
      // failures while React temporarily disables or duplicates an editor.
      return {
        authenticated:true,
        accountFingerprint:account.fingerprint,
        status:"idle",
        detail:null,
      };
    } catch (error) {
      const failure = asCompanionError(error);
      const authenticationRequired = failure.code === "authentication_required";
      if (authenticationRequired) this.healthAccount.invalidate();
      return {
        authenticated:false,
        accountFingerprint:null,
        status:authenticationRequired ? "authentication_required" : "unavailable",
        detail:failure.message,
      };
    }
  }

  async login(signal?:AbortSignal): Promise<{
    authenticated:boolean;
    accountFingerprint:string | null;
    status:"idle" | "busy" | "authentication_required" | "unavailable";
    detail:string | null;
  }> {
    this.planCache=undefined;
    this.healthAccount.invalidate();
    await this.surface.navigate("https://chatgpt.com/",signal);
    await this.surface.reveal();
    return this.health();
  }

  async prepareNewChat(projectUrl:string | null, signal?:AbortSignal): Promise<CapabilityCatalog> {
    const trace=this.preparationTrace('setup-chat');
    await this.surface.beginAction();
    try {
      trace('opening new chat');
      await openNewChat(this.surface, projectUrl,signal);
      trace('clearing setup draft');
      await clearExistingDraft(await this.surface.page(),signal);
      trace('checking setup account');
      const account=await readChatGptAccount(await this.surface.page(),signal);
      const cache=this.capabilitiesCache;
      // Batch-created workers share one account and usually prepare within a
      // minute. Reuse the catalogue hint, not model/effort authority: every
      // Send still verifies its actual live picker. Explicit refreshes below
      // always rescan, and account/plan changes never reuse this snapshot.
      if(cache&&cache.accountFingerprint===account.fingerprint&&cache.planType===account.planType
        &&Date.now()-cache.observedAtMs<60_000){trace('reusing verified catalogue');return structuredClone(cache.catalog);}
      return await this.capabilities(signal,trace);
    }
    finally { await this.surface.endAction(); }
  }

  async capabilities(signal?:AbortSignal,progress?:(name:string)=>void): Promise<CapabilityCatalog> {
    const trace=progress??this.preparationTrace('capabilities');
    signal?.throwIfAborted();
    let page = await this.surface.page();
    if (new URL(page.url()).origin !== "https://chatgpt.com") {
      page = await this.surface.navigate("https://chatgpt.com/",signal);
    }
    trace('checking session attention');
    await throwIfSessionAttention(page);
    trace('checking catalogue account');
    const account = await readChatGptAccount(page,signal);
    // Authentication is proven by the session API, not a composer that React
    // may not have mounted yet. Hydration gets one bounded readiness wait.
    trace('waiting for setup composer');
    await waitForConversationComposer(page,signal);
    const capabilities = await browserStage("capability_discovery","ChatGPT capability discovery failed",()=>detectChatGptAccountCapabilities(page,{signal,onProgress:trace}));
    const planKind = this.planCache?.plan.accountFingerprint === account.fingerprint&&this.planCache.planType===account.planType
      ? this.planCache.plan.kind : classifyPlan(account.planType).kind;
    const catalog = buildCapabilityCatalog(capabilities, account.fingerprint, this.build, planKind);
    this.capabilitiesCache = { accountFingerprint:account.fingerprint,planType:account.planType,observedAtMs:Date.now(),capabilities,catalog };
    trace('setup ready');
    return catalog;
  }

  async accountPlan(epoch:string, signal?:AbortSignal):Promise<AccountPlan> {
    signal?.throwIfAborted();
    const main=await this.surface.page(),account=await readChatGptAccount(main,signal);
    if(this.planCache?.epoch===epoch&&this.planCache.plan.accountFingerprint===account.fingerprint&&this.planCache.planType===account.planType)return this.planCache.plan;
    // This is polled automatically at startup. Visiting billing in the single
    // shared tab can hold Setup Chat and real work behind a 30-second UI probe.
    // The authenticated session supplies the plan family; exact model/effort
    // availability is still discovered and verified from the live picker.
    const plan={...classifyPlan(account.planType),accountFingerprint:account.fingerprint};
    this.planCache={epoch,planType:account.planType,plan};return plan;
  }

  async navigationAvailability(url:string):Promise<number|null> {
    return conversationLoadAvailableAt(await this.surface.page(),url);
  }

  async refreshConnector(name:string, signal?:AbortSignal) {
    return this.refreshConnectorSteps(name,async(operation,parent)=>operation(parent??new AbortController().signal),signal);
  }

  async refreshConnectorSteps(name:string,run:BrowserStepRunner,signal?:AbortSignal) {
    if(this.pendingRefresh)throw new BrowserCompanionError("Browser companion is busy with a connector refresh");
    let expectedAccount:string|undefined;
    const step = <T>(label:string,operation:(page:Page,account:ChatGptAccount,signal:AbortSignal)=>Promise<T>,maintenance=true)=>run(async operationSignal=>{
      operationSignal.throwIfAborted();
      const page=await this.surface.page(),account=await readChatGptAccount(page,operationSignal);
      if(expectedAccount&&expectedAccount!==account.fingerprint)throw new BrowserCompanionError("ChatGPT account changed during connector maintenance","authentication_required");
      expectedAccount=account.fingerprint;
      return maintenance?this.withMaintenancePage(page,account,main=>operation(main,account,operationSignal),operationSignal):operation(page,account,operationSignal);
    },signal).catch(error=>{
      if(error instanceof Error&&!(error instanceof BrowserCompanionError)){
        // Keep a bounded private diagnostic for framework failures that are
        // intentionally hidden behind the public generic operation error.
        const detail=error.message.split(/\r?\n/u,1)[0]!.replace(/https?:\/\/\S+/gu,'[url]')
          .replace(/[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}/gu,'[email]').slice(0,200);
        console.error(`[connector-maintenance] ${label}: ${error.name}: ${detail}`);
      }
      const failure=asCompanionError(error);
      throw new BrowserCompanionError(`${label}: ${failure.message}`,failure.code,{cause:error});
    });
    const appId=await step("Identify connector",async(page,account,signal)=>{
      const cached=this.connectorApps.get(account.fingerprint,name);
      if(cached)return cached;
      const discovered=await observedConnectorApp(page,name,signal)??await this.withMaintenancePage(page,account,main=>identifyConnectorApp(main,name,account.fingerprint,signal),signal);
      this.connectorApps.remember(account.fingerprint,name,discovered);return discovered;
    },false);
    if(this.pendingRefresh)throw new BrowserCompanionError("Browser companion is busy with a connector refresh");
    let originalUrl!:string;
    let pending:{page:Page;url:string;account:ChatGptAccount}|undefined;
    let observation:ConnectorRefreshObservation|undefined;
    let refreshFailure:unknown;
    const phase=<T>(label:string,operation:(page:Page,account:ChatGptAccount,signal:AbortSignal)=>Promise<T>)=>step(label,async(page,account,operationSignal)=>{
      if(this.pendingRefresh&&this.pendingRefresh!==pending)throw new BrowserCompanionError("Browser companion is busy with another connector refresh");
      if(pending&&(pending.page!==page||pending.url!==page.url()))throw new BrowserCompanionError("Another browser action changed pages during connector refresh","external_interference");
      if(!pending)originalUrl=page.url();
      const result=await this.withMaintenancePage(page,account,main=>operation(main,account,operationSignal),operationSignal,{restore:false});
      pending={page,url:page.url(),account};this.pendingRefresh=pending;
      return result;
    },false);
    const waitSignal=signal?AbortSignal.any([signal,AbortSignal.timeout(60_000)]):AbortSignal.timeout(60_000);
    let onAbort!:()=>void;
    const interrupted=new Promise<never>((_,reject)=>{
      onAbort=()=>reject(new BrowserCompanionError(signal?.aborted?"Refresh observation cancelled":"Tool refresh completion was not confirmed within 60 seconds",signal?.aborted?"cancelled":"failed"));
      if(waitSignal.aborted)onAbort();else waitSignal.addEventListener("abort",onAbort,{once:true});
    });
    void interrupted.catch(()=>{}); // Cancellation during preparation is handled by its bounded phase.
    try {
      const before=await phase("Inspect connector settings",(page,account,operationSignal)=>inspectConnectorSettings(page,appId,name,account.fingerprint,operationSignal));
      const knownSummary=this.connectorApps.getAllowAllSummary(expectedAccount!,name);
      const needsVerification=!before?.allowAll&&(!knownSummary||before?.summary!==knownSummary);
      let permissionChanged=false;
      if(needsVerification)permissionChanged=await phase("Verify connector permission",(page,account,operationSignal)=>setConnectorAllowAll(page,appId,account.fingerprint,operationSignal));
      observation=await phase("Start connector refresh",async(page,account,operationSignal)=>{
        if(needsVerification)await verifyConnectorAllowAll(page,appId,account.fingerprint,operationSignal);
        const current=await inspectConnectorSettings(page,appId,name,account.fingerprint,operationSignal);
        if(!needsVerification&&!current?.allowAll&&(!knownSummary||current?.summary!==knownSummary))throw new BrowserCompanionError("Connector permission changed before refresh","external_interference");
        if(current?.summary)this.connectorApps.rememberAllowAllSummary(account.fingerprint,name,current.summary);
        return beginConnectorToolRefresh(page,appId,name,account.fingerprint,operationSignal);
      });
      // The coordinator retains the shared window through confirmation and the
      // restoration below; other operations must not navigate away mid-refresh.
      const refreshObserved=await Promise.race([observation.wait(waitSignal),interrupted]);
      return {appId,permissionChanged,refreshObserved,schemaVerified:false as const};
    }catch(error){refreshFailure=error;throw error;}
    finally {
      waitSignal.removeEventListener("abort",onAbort);
      // Restore only if the shared page is still ours. Never undo another
      // action's navigation or overwrite a draft typed while refresh waited.
      try {await run(async operationSignal=>{
        const page=await this.surface.page();
        if(!pending||page!==pending.page||page.url()!==pending.url)return;
        await observation?.dispose();
        if(page.url()===originalUrl||await observeReadOnly(page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true}).count(),operationSignal))return;
        await this.surface.beginAction();
        try{await this.surface.navigate(originalUrl,operationSignal);}finally{await this.surface.endAction();}
      });}catch(error){
        // An intervening submission can own the page while observation ends.
        if(!refreshFailure&&!(error instanceof Error&&error.message.toLowerCase().includes("busy"))
          && !(error instanceof BrowserCompanionError&&error.code==="conversation_cooldown")) {
          const failure=asCompanionError(error);
          throw new BrowserCompanionError(`Restore connector page: ${failure.message}`,failure.code,{cause:error});
        }
      }finally{if(this.pendingRefresh===pending)this.pendingRefresh=undefined;}
    }
  }

  private async withMaintenancePage<T>(page:Page,account:ChatGptAccount,operation:(page:Page)=>Promise<T>,signal?:AbortSignal,options:{restore?:boolean;clearDraft?:boolean}={}):Promise<T> {
    signal?.throwIfAborted();
    await this.surface.beginAction();
    const previousUrl=page.url();
    let started=false;
    let completed=false;
    try {
      try { await observeReadOnly(throwIfSessionAttention(page),signal); }
      catch(error) {
        // A page-local load failure must not prevent opening global settings.
        if (!(error instanceof BrowserCompanionError && ["conversation_unavailable","conversation_limit","rate_limited"].includes(error.code))) throw error;
      }
      // Ownership tracks our UI operation, never ChatGPT's response lifetime.
      const composers=page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true});
      const count=await observeReadOnly(composers.count(),signal);
      if(count>1)throw new BrowserCompanionError("Browser companion is busy; the shared composer is ambiguous");
      if(await this.surface.page()!==page||page.url()!==previousUrl)throw new BrowserCompanionError("ChatGPT changed pages before account maintenance", "external_interference");
      this.maintenance={page,account};started=true;
      signal?.throwIfAborted();
      if(count===1&&options.clearDraft!==false)await clearExistingDraft(page,signal);
      const result=await operation(page);completed=true;return result;
    } finally {
      try {
        if(started&&(options.restore!==false||!completed)&&page.url()!==previousUrl)await this.surface.navigate(previousUrl,AbortSignal.timeout(signal?.aborted?1_500:8_000));
      } finally {
        this.maintenance=undefined;
        await this.surface.endAction();
      }
    }
  }

  async reveal(url: string | null, signal?:AbortSignal): Promise<void> {
    signal?.throwIfAborted();
    if (url) {
      const page=await this.surface.page();let same=page.url()===url;
      try {same ||= canonicalChatGptConversationUrl(page.url())===canonicalChatGptConversationUrl(url);}catch{}
      if(!same)await this.surface.navigate(url,signal);
    }
    await this.surface.reveal();
  }

  async recoverAuthentication(request:ExecuteActionRequest,signal:AbortSignal):Promise<void> {
    signal.throwIfAborted();
    await this.surface.beginAction();
    try {
      const page=await this.surface.page();
      // Preserve a pending approval rather than discard its in-memory card.
      await throwIfConversationApproval(page);
      await navigatePage(page,request.url,{signal,reload:page.url()===request.url});
      await waitForConversationComposer(page,signal);
    } finally {await this.surface.endAction();}
  }

  async replacePairingDraft(request: ExecuteActionRequest, signal?:AbortSignal): Promise<boolean> {
    signal?.throwIfAborted();
    if (request.prompt.kind !== "pair_verify") return false;
    const page = await this.surface.page();
    if (canonicalChatGptConversationUrl(page.url()) !== canonicalChatGptConversationUrl(request.expectedUrl)
      || await generationRunning(page) || await toolApprovalVisible(page)) return false;
    const account = await readChatGptAccount(page);
    if (account.fingerprint !== request.expectedAccountFingerprint) return false;
    const composer = await activeComposer(page, 2_000);
    if (!isPairingVerificationDraft(await composerText(composer))
      || await composer.locator(SELECTED_CONNECTOR_SELECTOR).filter({ visible:true }).count() !== 1) return false;
    await clearComposerState(page, request.connectorName);
    return true;
  }

  async turnHandoffComplete(url:string,account:string,handoffId:string,signal?:AbortSignal):Promise<boolean> {
    await this.surface.beginAction();
    try {
      const target=canonicalChatGptConversationUrl(url),key=`${account}\u001f${target}\u001f${handoffId}`;
      let page=await this.surface.page(),same=false;
      try {same=canonicalChatGptConversationUrl(page.url())===target;}catch{}
      if(!same){
        if((await observeReadOnly(readChatGptAccount(page),signal)).fingerprint!==account)throw new BrowserCompanionError('ChatGPT account changed before handoff inspection','authentication_required');
        if(Date.now()<(this.handoffProbes.get(key)??0))throw new BrowserCompanionError('Handoff inspection is waiting for its next real probe','editor_unavailable');
        this.handoffProbes.delete(key);this.handoffProbes.set(key,Date.now()+1_000);
        if(this.handoffProbes.size>256)this.handoffProbes.delete(this.handoffProbes.keys().next().value!);
        // A bounded visit can discover a final after another chat took the tab.
        // Repeated polls of the current conversation never reload its document.
        page=await this.surface.navigate(url,signal);
      }
      await observeReadOnly(throwIfSessionAttention(page),signal);
      await observeReadOnly(throwIfConversationApproval(page),signal);
      if(canonicalChatGptConversationUrl(page.url())!==target)throw new BrowserCompanionError('ChatGPT changed conversations during handoff inspection','conversation_unavailable');
      if((await observeReadOnly(readChatGptAccount(page),signal)).fingerprint!==account)throw new BrowserCompanionError('ChatGPT account changed during handoff inspection','authentication_required');
      const complete=await observeReadOnly(completedHandoff(page,url,account,handoffId,signal),signal,10_000);
      if(complete)this.handoffProbes.delete(key);
      return complete;
    } finally {await this.surface.endAction();}
  }

  async execute(execution: DriverExecution): Promise<void> {
    execution.signal.throwIfAborted();
    this.pendingSubmission={actionId:execution.request.actionId,mayHaveSent:false};
    await this.surface.beginAction();
    let sendActivated = false;
    let sendAccepted = false;
    let submissionSettlesAt = 0;
    let submissionNetwork:ReturnType<typeof observeSubmissionNetwork>|undefined;
    let submissionSlot:Awaited<ReturnType<typeof acquireSubmissionSlot>>|undefined;
    const settleSubmission = async () => {
      const remaining = submissionSettlesAt - Date.now();
      // Keep ownership even if cancelled after Send: navigating away can abort
      // ChatGPT's request after its UI already reported acceptance.
      if (remaining > 0) await new Promise(resolve => setTimeout(resolve,remaining));
    };
    let ownedPage: Page | undefined;
    let insertionAttempted = false;
    let selectedConnector: SelectedConnector | undefined;
    const connectorMatches = async (composer:Locator):Promise<boolean> => selectedConnector
      ? connectorIsSelected(composer,execution.request.connectorName,selectedConnector.identity)
      : await composer.locator(SELECTED_CONNECTOR_SELECTOR).filter({visible:true}).count()===0;
    let pairingSelection: PairingMediumSelection | undefined;
    let restoreUrl: string | undefined;
    let submissionStep = "opening conversation";
    const preparationStarted=performance.now();
    const timing={actionId:execution.request.actionId,totalMs:0,steps:[] as Array<{name:string;atMs:number}>};
    this.preparationTimings.push(timing);
    if(this.preparationTimings.length>8)this.preparationTimings.shift();
    const step = (name:string) => {
      submissionStep = name;
      timing.totalMs=Math.round(performance.now()-preparationStarted);
      timing.steps.push({name,atMs:timing.totalMs});
      if (submissionTracingEnabled()) console.error(`[submission-step] ${execution.request.actionId} ${name}`);
    };
    try {
      const creating = execution.request.prompt.kind === "create_chat";
      if (!creating) {
        const current = await this.surface.page();
        let sameConversation = false;
        try { sameConversation = canonicalChatGptConversationUrl(current.url()) === canonicalChatGptConversationUrl(execution.request.expectedUrl); } catch {}
        // Reloading can discard an in-memory approval card. Preserve it for the
        // conversation approval handler instead of restarting the blocked turn.
        if (sameConversation) {
          if ((await readChatGptAccount(current)).fingerprint !== execution.request.expectedAccountFingerprint) {
            throw new BrowserCompanionError("ChatGPT account changed before the action could start", "authentication_required");
          }
          await throwIfConversationApproval(current);
        }
      }
      execution.emit({ type:"navigating" });
      step("loading conversation");
      const navigationProgress=(stage:"document_loaded"|"editor_ready")=>execution.emit({type:"navigation_progress",step:stage});
      const opened = creating ? await openNewChat(this.surface, execution.request.url === "https://chatgpt.com/new" ? null : execution.request.url, execution.signal,navigationProgress) : null;
      const exact = opened ? {page:opened, canonicalUrl:opened.url()} : await exactConversationPage(this.surface, execution.request.url, execution.request.expectedUrl, execution.signal,navigationProgress);
      execution.signal.throwIfAborted();
      const page = exact.page;
      let canonicalUrl = exact.canonicalUrl;
      restoreUrl = canonicalUrl;
      step("checking account");
      const account = await readChatGptAccount(page);
      if (account.fingerprint !== execution.request.expectedAccountFingerprint) {
        throw new BrowserCompanionError("ChatGPT account changed before the action could start", "authentication_required");
      }
      execution.emit({type:"navigation_progress",step:"account_verified"});
      ownedPage = page;
      await throwIfConversationApproval(page);
      recordConversationAvailable(page);
      const renewal=execution.request.prompt.kind === "turn_renewal" ? execution.request.prompt : null;
      const verifyRenewal=renewal?createCompletedHandoffVerifier(page,canonicalUrl,account.fingerprint,renewal.handoffId,execution.signal):undefined;
      const verifyHandoff=async()=>{
        if(verifyRenewal && !await verifyRenewal()) {
          throw new BrowserCompanionError("Turn handoff is not complete; continuation was not sent","external_interference");
        }
      };
      const canStopPrevious = execution.request.prompt.kind !== "pair_verify" && !creating && !renewal;
      const stopPrevious = async () => {
        if (canStopPrevious) await stopLingeringResponse(page, execution.signal, { canonicalUrl, accountFingerprint:account.fingerprint }, execution.emit);
      };
      const prepareBeforeSend = async <T>(operation:()=>Promise<T>):Promise<T> => {
        await stopPrevious();
        try { return await operation(); }
        catch (error) {
          // React can open the confirmation after the initial Stop inspection,
          // while a picker or composer operation is already in progress. Resolve
          // that exact modal, then retry only this pre-Send preparation step.
          if (!canStopPrevious || !(await visibleStopDialogs(page)).length) throw error;
          await stopPrevious();
          return operation();
        }
      };
      await verifyHandoff();
      if (execution.request.prompt.kind === "pair_verify") await this.replacePairingDraft(execution.request);
      if(this.recoveryEnhancements && !creating && execution.request.prompt.kind!=='pair_verify'){
        const draft=await composerText(await activeComposer(page,3_000,execution.signal));
        const staleConnectorQuery=draft.trim()===`@${execution.request.connectorName}`;
        const ownedUnsent=this.draftRecovery.recoverable(account.fingerprint,canonicalUrl,draft);
        const ownedHandoffDraft=Boolean(renewal&&this.draftRecovery.recoverableHandoffDraft(account.fingerprint,canonicalUrl,draft));
        if(draft && ((!renewal&&isAutomationGoalDraft(draft))||staleConnectorQuery||ownedUnsent||ownedHandoffDraft)){
          // This fresh native request has not clicked Send. A previous browser
          // deadline/restart can leave fixed goal-control prose behind; its exact
          // identity is recoverable without discarding user text or replaying a batch.
          if(ownedHandoffDraft)await verifyHandoff();
          await clearExistingDraft(page,execution.signal,draft);
          console.warn(`[composer-recovery] reclaimed ${ownedUnsent?'confirmed unsent draft':staleConnectorQuery?'connector query':'fixed automation goal draft'} chars=${draft.length}`);
        }
      }
      if (canStopPrevious) {
        // Offline actions are dispatched after tool-call inactivity. Stop only
        // while preparing this send; ongoing conversations need no observation.
        step("stopping previous response");
        await traceStopControls(page,"before-stop");
        await stopPrevious();
        if (canonicalChatGptConversationUrl(page.url()) !== canonicalUrl) {
          throw new BrowserCompanionError("ChatGPT changed conversations while stopping the previous response", "conversation_unavailable");
        }
      } else if(creating||execution.request.prompt.kind==="pair_verify") {
        await clearExistingDraft(page,execution.signal);
      }
      const cache = this.capabilitiesCache;
      step("checking draft");
      if(!creating&&execution.request.prompt.kind!=="pair_verify")
        await prepareBeforeSend(async()=>ensureEmptyOwnedComposer(await activeComposer(page,3_000,execution.signal)));
      else await clearExistingDraft(page,execution.signal);
      execution.emit({ type:"composer_ready" });
      const verifyFallbackDestination=async()=>{
        execution.signal.throwIfAborted();
        if (canonicalChatGptConversationUrl(page.url())!==canonicalChatGptConversationUrl(canonicalUrl)) {
          throw new BrowserCompanionError("ChatGPT changed conversations during model fallback","conversation_unavailable");
        }
        if ((await readChatGptAccount(page)).fingerprint!==execution.request.expectedAccountFingerprint) {
          throw new BrowserCompanionError("ChatGPT account changed during model fallback","authentication_required");
        }
      };
      let selected: Awaited<ReturnType<typeof selectAvailableModelAndEffort>> | null = null;
      let editorRecovered=false;
      const recoverEditor=async()=>{
        if(!this.recoveryEnhancements||editorRecovered)return false;
        editorRecovered=true;
        step("refreshing stuck editor");
        if(pairingSelection) {
          await this.restorePairingSelection(page,pairingSelection,canonicalUrl,execution.request.expectedAccountFingerprint);
          pairingSelection=undefined;
        }
        await page.keyboard.press("Escape");
        await refreshConversationEditor(page,canonicalUrl,account.fingerprint,execution.signal,
          recoveryStep=>execution.emit({type:"editor_recovery",step:recoveryStep}),
          creating?{projectUrl:execution.request.url==="https://chatgpt.com/new"?null:execution.request.url}:undefined);
        if(creating){canonicalUrl=page.url();restoreUrl=canonicalUrl;}
        if(creating||execution.request.prompt.kind==="pair_verify")pairingSelection=await selectPairingMedium(page,execution.signal);
        await verifyHandoff();
        return true;
      };
      const selectModel=async()=>{
        if(execution.request.model===null||execution.request.effort===null)return null;
        step("selecting model");
        const model=execution.request.model,effort=execution.request.effort;
        return prepareBeforeSend(()=>selectAvailableModelAndEffort(page,model,effort,
          cache?.accountFingerprint===account.fingerprint?cache.capabilities:undefined,execution.signal,verifyFallbackDestination,true));
      };
      if (!creating && execution.request.prompt.kind !== "pair_verify"
        && execution.request.model !== null && execution.request.effort !== null) {
        try {selected=await selectModel();}
        catch(error) {
          if(!(error instanceof BrowserCompanionError)||error.code!=="model_unavailable"
            ||error instanceof ModelOptionUnavailableError||!await recoverEditor())throw error;
          selected=await selectModel();
        }
        execution.emit({ type:"model_verified", model:selected!.id, effort:selected!.effort });
      }
      if (creating || execution.request.prompt.kind === "pair_verify") {
        step("selecting pairing Medium");
        pairingSelection = await selectPairingMedium(page, execution.signal);
      }
      // A verified existing conversation already carries its connector tools.
      // Reopening the app picker for every wake can hang React on long histories.
      // Preserve a selected chip if present; require selection for new/pairing
      // conversations, where tool availability has not yet been established.
      const requiresConnector=creating||execution.request.prompt.kind==="pair_verify";
      const hasConnectorChip=await (await activeComposer(page,3_000,execution.signal))
        .locator(SELECTED_CONNECTOR_SELECTOR).filter({visible:true}).count()>0;
      if(!requiresConnector&&!hasConnectorChip)execution.emit({type:"connector_omitted"});
      for(let attempt=0;(requiresConnector||hasConnectorChip)&&attempt<2;attempt++) {
        try {
          step("selecting connector");
          selectedConnector=await prepareBeforeSend(()=>selectConnector(page,execution.request.connectorName,execution.signal,
            connectorStep=>{step(`connector ${connectorStep}`);execution.emit({type:"connector_progress",step:connectorStep});}));
          break;
        } catch(error) {
          if(!(error instanceof BrowserCompanionError)||error.code!=="connector_unavailable")throw error;
          execution.signal.throwIfAborted();
          if(attempt===0&&await recoverEditor()) {
            if(selected){selected=await selectModel();execution.emit({type:"model_verified",model:selected!.id,effort:selected!.effort});}
            continue;
          }
          if(creating||execution.request.prompt.kind==="pair_verify")throw error;
          execution.emit({type:"connector_omitted"});
          break;
        }
      }
      if(selectedConnector) {
        this.connectorApps.remember(account.fingerprint,execution.request.connectorName,selectedConnector.identity);
        execution.emit({ type:"connector_verified" });
      }
      // Read history before inserting a prompt: unsupported markers must not
      // make a newly filled composer flash and disappear during failure cleanup.
      step("reading submission baseline");
      await verifyHandoff();
      await throwIfConversationApproval(page);
      if(!creating&&execution.request.prompt.kind!=="pair_verify"&&await generationRunning(page)) {
        if(!this.recoveryEnhancements)throw new BrowserCompanionError("ChatGPT's previous turn is still running; the continuation was not sent","editor_unavailable");
        // A control can arrive while the model/app picker is being prepared.
        // Stop before inserting our prompt; preserve that selected UI in place.
        step("stopping late previous response");
        await stopLingeringResponse(page,execution.signal,{canonicalUrl,accountFingerprint:account.fingerprint},execution.emit,
          {mayReload:false,budgetMs:20_000});
      }
      let baseline = await captureSubmissionBaseline(page, execution.renderedPrompt, execution.signal);
      this.draftRecovery.prepare(account.fingerprint,canonicalUrl,execution.request.actionId,execution.renderedPrompt);
      insertionAttempted = true;
      step("inserting prompt");
      const preparedComposer=selectedConnector?.composer ?? await activeComposer(page,30_000,execution.signal);
      try {await prepareBeforeSend(()=>insertExactPrompt(preparedComposer, execution.renderedPrompt));}
      catch(error){
        execution.signal.throwIfAborted();
        await throwIfConversationApproval(page);
        if(error instanceof BrowserCompanionError)throw error;
        throw new BrowserCompanionError("ChatGPT did not accept the message in its editor; nothing was sent","editor_unavailable",{cause:error});
      }
      step("waiting for Send control");
      let send = await uniqueSendButton(preparedComposer, 30_000, execution.signal,
        () => throwIfConversationApproval(page));
      await throwIfConversationApproval(page);
      step("checking model and connector");
      if (pairingSelection) await pairingSelection.verify(execution.signal);
      else if (selected) await assertSelectedModelAndEffort(page, selected, execution.signal, creating);
      // Locators resolve against the current DOM after any picker rerender.
      // Revalidate uniqueness/editability here once, then read the connector
      // and exact draft from that same locator without repeated readiness RPCs.
      const finalComposer=await activeComposer(page,30_000,execution.signal);
      if (!await connectorMatches(finalComposer)) {
        throw new BrowserCompanionError("ChatGPT changed the selected connector before Send", "external_interference");
      }
      step("checking prompt and conversation");
      submissionSlot=await acquireSubmissionSlot(page,execution.signal);
      // Another page may have prepared a model or the account may have changed
      // while this page waited for the shared Send boundary.
      if((await readChatGptAccount(page,execution.signal)).fingerprint!==execution.request.expectedAccountFingerprint)
        throw new BrowserCompanionError("ChatGPT account changed before Send","authentication_required");
      if(pairingSelection)await pairingSelection.verify(execution.signal);
      else if(selected)await assertSelectedModelAndEffort(page,selected,execution.signal,creating);
      if(!await connectorMatches(await activeComposer(page,3_000,execution.signal)))
        throw new BrowserCompanionError("ChatGPT changed the selected connector before Send","external_interference");
      if (await composerText(finalComposer) !== execution.renderedPrompt) {
        throw new BrowserCompanionError("ChatGPT composer text changed before Send", "external_interference");
      }
      if (creating) await assertNewChatDraft(page, execution.request.url === "https://chatgpt.com/new" ? null : execution.request.url);
      else if (canonicalChatGptConversationUrl(page.url()) !== canonicalUrl) {
        throw new BrowserCompanionError("ChatGPT changed the verified conversation before Send", "conversation_unavailable");
      }
      await throwIfConversationApproval(page);
      execution.signal.throwIfAborted();
      await verifyHandoff();
      await throwIfConversationLimit(page);
      if(!creating&&execution.request.prompt.kind!=="pair_verify"&&await generationRunning(page)) {
        if(!this.recoveryEnhancements || renewal || !this.draftRecovery.recoverable(account.fingerprint,canonicalUrl,await composerText(await activeComposer(page,3_000,execution.signal))))
          throw new BrowserCompanionError("ChatGPT's previous turn is still running; the continuation was not sent","editor_unavailable");
        // Work can reveal End turn only after editor input. Nothing has clicked
        // Send: remove only our exact, journal-owned prompt, stop in place once,
        // then rebuild the baseline and all pre-Send proof against the new DOM.
        step("stopping response revealed after insertion");
        await verifyFallbackDestination();
        await throwIfConversationApproval(page);
        if(await composerText(await activeComposer(page,3_000,execution.signal))!==execution.renderedPrompt)
          throw new BrowserCompanionError('ChatGPT draft was edited before Stop recovery','external_interference');
        await clearComposerState(page,execution.request.connectorName,execution.signal);
        await stopLingeringResponse(page,execution.signal,{canonicalUrl,accountFingerprint:account.fingerprint},execution.emit,{mayReload:false,budgetMs:20_000});
        await ensureEmptyOwnedComposer(await activeComposer(page,3_000,execution.signal));
        if(selectedConnector)selectedConnector=await selectConnector(page,execution.request.connectorName,execution.signal);
        baseline=await captureSubmissionBaseline(page,execution.renderedPrompt,execution.signal);
        await insertExactPrompt(await activeComposer(page,3_000,execution.signal),execution.renderedPrompt);
        send=await uniqueSendButton(await activeComposer(page,3_000,execution.signal),10_000,execution.signal,()=>throwIfConversationApproval(page));
        await verifyFallbackDestination();
        await throwIfConversationApproval(page);
        if(selected)await assertSelectedModelAndEffort(page,selected,execution.signal);
        const composer=await activeComposer(page,3_000,execution.signal);
        if(await composerText(composer)!==execution.renderedPrompt || !await connectorMatches(composer))
          throw new BrowserCompanionError('ChatGPT changed the prepared continuation after Stop','external_interference');
        if(await generationRunning(page))throw new BrowserCompanionError("ChatGPT's previous turn is still running; the continuation was not sent","editor_unavailable");
      }
      if(this.surface.requireSubmissionNetworkProof)submissionNetwork=observeSubmissionNetwork(page);
      submissionSlot.markSent();
      this.draftRecovery.beforeSend(execution.request.actionId);
      sendActivated = true;
      if(this.pendingSubmission?.actionId===execution.request.actionId)this.pendingSubmission.mayHaveSent=true;
      step("clicking Send");
      submissionSettlesAt = Date.now() + 5_000;
      await send.click({ timeout:10_000, signal:execution.signal });
      submissionSlot.release();submissionSlot=undefined;
      execution.emit({type:"submission_progress",step:"send_clicked"});
      submissionSettlesAt = Date.now() + 5_000;
      step("confirming Send acceptance");
      try {
        await Promise.all([
          waitForSubmissionAcceptance(page, baseline, Date.now() + 25_000, execution.signal),
          submissionNetwork?.wait(execution.signal),
        ]);
      } catch (error) {
        if(error instanceof BrowserCompanionError&&error.code==="conversation_limit")throw error;
        throw new BrowserCompanionError(
          error instanceof BrowserCompanionError ? error.message : "ChatGPT Send may have been accepted, but semantic acceptance could not be proven",
          "ambiguous_send",
          { sendAccepted:true, cause:error },
        );
      }
      sendAccepted = true;
      this.draftRecovery.forget(execution.request.actionId);
      execution.emit({ type:"send_accepted" });
      if (creating) {
        const created = new URL(await createdChatUrl(page, execution.signal)); created.search = ""; created.hash = ""; canonicalUrl = created.href;
        // Persist the actual chat identity before model restoration can fail or time out.
        execution.emit({type:"conversation_created",canonicalUrl});
      }
      restoreUrl = canonicalUrl;
      if (pairingSelection) {
        await this.restorePairingSelection(page, pairingSelection, restoreUrl, execution.request.expectedAccountFingerprint);
        pairingSelection = undefined;
      }
      if (creating && execution.request.model !== null && execution.request.effort !== null) {
        // The initialization message only borrows Medium. Apply the user's
        // conversation preferences after acceptance, never to the pairing send.
        if (canonicalChatGptConversationUrl(page.url()) !== canonicalChatGptConversationUrl(canonicalUrl)
          || (await readChatGptAccount(page)).fingerprint !== execution.request.expectedAccountFingerprint) {
          throw new BrowserCompanionError("ChatGPT changed the new conversation before applying its settings", "external_interference");
        }
        selected = await selectAvailableModelAndEffort(page, execution.request.model, execution.request.effort,
          cache?.accountFingerprint === account.fingerprint ? cache.capabilities : undefined, execution.signal,verifyFallbackDestination);
        await assertSelectedModelAndEffort(page, selected, execution.signal);
        execution.emit({ type:"model_verified", model:selected.id, effort:selected.effort });
      }
      step("letting submission requests settle");
      await settleSubmission();
      execution.signal.throwIfAborted();
      await throwIfConversationLimit(page);
      execution.emit({ type:"submission_complete", canonicalUrl });
      timing.totalMs=Math.round(performance.now()-preparationStarted);
    } catch (error) {
      if(ownedPage&&error instanceof BrowserCompanionError&&error.message.includes("routed the message into the previous turn")) {
        await traceStopControls(ownedPage,"unexpected-steer");
        if(this.recoveryEnhancements && execution.request.prompt.kind==="offline_goal") {
          // A stale backend turn may expose Stop only after a steering request.
          // End that turn through its verified UI, but retain the ambiguous send:
          // no reload, draft clearing, acceptance claim, or same-action resend.
          try {
            await settleSubmission();
            await stopLingeringResponse(ownedPage,execution.signal,{
              canonicalUrl:canonicalChatGptConversationUrl(execution.request.expectedUrl),
              accountFingerprint:execution.request.expectedAccountFingerprint,
            },event=>{if(event.type==="stop_progress")execution.emit(event);},{mayReload:false,budgetMs:20_000});
            await traceStopControls(ownedPage,"after-steer-stop");
          } catch { /* Preserve the original uncertain-send result. */ }
        }
      }
      if (submissionTracingEnabled()) {
        const details=(error instanceof AggregateError?error.errors:[error]).map(failure=>{
          const value=asCompanionError(failure);
          return `${value.code}: ${value.message.split(/\r?\n/)[0]!.slice(0,240)}`;
        }).join("; ");
        console.error(`[submission-step] ${execution.request.actionId} failed at ${submissionStep}; sendActivated=${sendActivated}; sendAccepted=${sendAccepted}; ${details}`);
      }
      if(error instanceof BrowserCompanionError&&error.code==="conversation_limit") {
        const current=await this.surface.page();
        if(execution.request.prompt.kind!=="create_chat"&&canonicalChatGptConversationUrl(current.url())!==canonicalChatGptConversationUrl(execution.request.expectedUrl)) {
          throw new BrowserCompanionError("ChatGPT changed conversations before its length limit could be verified","external_interference",{sendAccepted:sendActivated});
        }
        if((await readChatGptAccount(current)).fingerprint!==execution.request.expectedAccountFingerprint) {
          throw new BrowserCompanionError("ChatGPT account changed before its conversation limit could be verified","authentication_required",{sendAccepted:sendActivated});
        }
        throw new BrowserCompanionError(error.message,"conversation_limit",{sendAccepted:sendAccepted,cause:error});
      }
      if (!sendActivated && insertionAttempted && ownedPage) {
        // A failed readiness check must not leave our own unchanged prompt blocking retries.
        // Never clear a draft that was edited, moved, or might already have been submitted.
        try {
          if ((execution.request.prompt.kind === "create_chat" ? ownedPage.url() === restoreUrl : canonicalChatGptConversationUrl(ownedPage.url()) === canonicalChatGptConversationUrl(execution.request.expectedUrl))
            && !await generationRunning(ownedPage)) {
            const composer = await activeComposer(ownedPage, 2_000);
            if (await composerText(composer) === execution.renderedPrompt
              && await connectorMatches(composer)) {
              await clearComposerState(ownedPage, execution.request.connectorName);
              this.draftRecovery.forget(execution.request.actionId);
            }
          }
        } catch { /* Preserve the original failure when safe cleanup is unavailable. */ }
      }
      if (!sendActivated && execution.request.prompt.kind !== "create_chat" && ownedPage
        && !(error instanceof BrowserCompanionError && error.code === "authentication_required")) {
        let sameTarget = false;
        try {sameTarget = canonicalChatGptConversationUrl(ownedPage.url()) === canonicalChatGptConversationUrl(execution.request.expectedUrl);} catch {}
        // A card can appear while a model/connector operation is awaiting UI.
        // Report that blocker so the supervisor resolves it on this page.
        if (sameTarget) await throwIfConversationApproval(ownedPage);
      }
      if (sendActivated && !sendAccepted && !(error instanceof BrowserCompanionError && error.code === "ambiguous_send")) {
        throw new BrowserCompanionError(
          asCompanionError(error).message,
          "ambiguous_send",
          { sendAccepted:true, cause:error },
        );
      }
      if (sendAccepted && error instanceof BrowserCompanionError && !error.sendAccepted) {
        throw new BrowserCompanionError(error.message, error.code, { sendAccepted:true, cause:error });
      }
      if(!sendActivated && submissionStep==="loading conversation" && execution.request.prompt.kind!=="create_chat"
        && ((error instanceof Error && error.name==="TimeoutError")
          || (error instanceof BrowserCompanionError && error.code==="editor_unavailable"))) {
        throw new BrowserCompanionError(CONVERSATION_UNAVAILABLE_MESSAGE,"conversation_unavailable",{cause:error});
      }
      throw error;
    } finally {
      submissionSlot?.release();
      try {
        await settleSubmission();
        if (pairingSelection && ownedPage && restoreUrl) {
          try { await this.restorePairingSelection(ownedPage, pairingSelection, restoreUrl, execution.request.expectedAccountFingerprint); }
          catch (error) {
            throw new BrowserCompanionError("Could not restore the conversation's model and effort after the one-off pairing request", "model_unavailable", {sendAccepted:sendActivated, cause:error});
          }
        }
      } finally {
        submissionNetwork?.close();
        if(this.pendingSubmission?.actionId===execution.request.actionId)this.pendingSubmission=undefined;
        await this.surface.endAction();
      }
    }
  }

  submissionMayHaveBeenAccepted(actionId:string):boolean {
    return this.pendingSubmission?.actionId===actionId && this.pendingSubmission.mayHaveSent;
  }

  private async restorePairingSelection(page: Page, selection: PairingMediumSelection, url: string, account: string): Promise<void> {
    let sameTarget = page.url() === url;
    if (!sameTarget) {
      try { sameTarget = canonicalChatGptConversationUrl(page.url()) === canonicalChatGptConversationUrl(url); }
      catch { /* A fresh top-level chat has no conversation ID until Send. */ }
    }
    if (!sameTarget
      || (await readChatGptAccount(page)).fingerprint !== account) {
      throw new BrowserCompanionError("ChatGPT changed the pairing target before restoring its settings", "external_interference");
    }
    await selection.restore(AbortSignal.timeout(10_000));
  }

  async close(): Promise<void> {
    this.healthAccount.dispose();
    await (this.surface.closeActionPage?.()??this.surface.close());
  }

}
