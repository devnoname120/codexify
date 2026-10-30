import type { Frame, Locator, Page } from "patchright";
import {completedHandoff} from "./turn-renewal";
import {classifyPlan,completeAccountPlan,readBillingPlan,identifyConnectorApp,observedConnectorApp,inspectConnectorSettings,setConnectorAllowAll,verifyConnectorAllowAll,beginConnectorToolRefresh,type ConnectorRefreshObservation,type AccountPlan} from "./account-settings";
import {ConnectorAppCache} from "./connector-cache";
import {approveConversationCard,throwIfConversationApproval} from "./conversation-approval";
import {openNewChat, createdChatUrl, assertNewChatDraft} from "./new-chat";
import { HealthAccountCache, readChatGptAccount, type ChatGptAccount } from "./account";
import { throwIfSessionAttention, throwIfConversationLimit, toolApprovalVisible } from "./browser-session";
import {navigatePage,recordConversationAvailable,conversationLoadAvailableAt} from "./navigation";
import type { BrowserDriver, DriverExecution, BrowserStepRunner, StopProgressStep } from "./browser-window";
import { activeComposer, composerText, insertExactPrompt, isPairingVerificationDraft, uniqueSendButton } from "./composer";
import { clearComposerState, connectorIsSelected, ConnectorMenuUnavailableError, SELECTED_CONNECTOR_SELECTOR, selectConnector, type SelectedConnector } from "./connector-selection";
import { asCompanionError, BrowserCompanionError,ConversationCooldownError,browserStage } from "./errors";
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
  page(): Promise<Page>;
  navigate(url: string, signal?: AbortSignal): Promise<Page>;
  reveal(): Promise<void>;
  beginAction(): Promise<void>;
  endAction(): Promise<void>;
  close(): Promise<void>;
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

async function generationRunning(page: Page): Promise<boolean> {
  return page.locator(CHATGPT_STOP_BUTTON_SELECTOR).filter({ visible:true }).count()
    .then(count => count > 0)
    .catch(() => false);
}

async function clearExistingDraft(page:Page,signal?:AbortSignal):Promise<Locator> {
  const composer=await activeComposer(page,30_000,signal);
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

async function stopLingeringResponse(
  page: Page,
  signal: AbortSignal,
  expected: { canonicalUrl:string; accountFingerprint:string },
  emit: DriverExecution["emit"],
): Promise<void> {
  if (signal.aborted) throw new DOMException("Browser action aborted", "AbortError");
  if (!await generationRunning(page) && !(await visibleStopDialogs(page)).length) return;
  const settleMs = 20_000;
  // Three bounded Stop attempts, one optional confirmation, and recovery work.
  const recoveryMs = settleMs * 4 + 10_000;
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
  const assertTarget = async () => {
    await throwIfSessionAttention(page);
    await throwIfConversationApproval(page);
    await assertAuthenticatedChatGptPage(page);
    if (canonicalChatGptConversationUrl(page.url()) !== expected.canonicalUrl) {
      throw new BrowserCompanionError("ChatGPT changed conversations while stopping the previous response", "conversation_unavailable");
    }
    if ((await readChatGptAccount(page)).fingerprint !== expected.accountFingerprint) {
      throw new BrowserCompanionError("ChatGPT account changed while stopping the previous response", "authentication_required");
    }
  };
  let confirmationSubmitted = false;
  const waitForStopped = async (stop:Locator) => {
    let settleDeadline = Math.min(deadline,(lastStopRequestedAt||Date.now())+settleMs);
    while (!boundedSignal.aborted) {
      const dialogs = await visibleStopDialogs(page);
      const count = dialogs.length;
      if (count) {
        const dialog = dialogs[0]!;
        // A previous action or the user may already have opened this dialog.
        // Its generic shell also serves other actions: require the exact shape
        // and a localized Stop submit button before confirming it.
        if (count !== 1 || !await dialog.evaluate(element => {
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
        })) {
          throw new BrowserCompanionError("ChatGPT has an unexpected dialog while stopping the response", "external_interference");
        }
        const confirm = dialog.locator('form button[type="submit"].bg-primary-solid')
          .and(dialog.getByRole('button',{name:uiLabelPattern('stopControl')}));
        if (await confirm.count() !== 1) {
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
          settleDeadline = Math.min(deadline,lastStopRequestedAt+settleMs);
        }
      }
      // Confirmation can remove its entire iframe. Re-scan on the next poll
      // instead of waiting on a detached locator and retrying Stop prematurely.
      if (!count && !await stop.isVisible()) return;
      if (Date.now() >= settleDeadline) throw new Error("ChatGPT Stop has not completed");
      await new Promise(resolve => setTimeout(resolve,50));
    }
    boundedSignal.throwIfAborted();
  };
  let lastError:unknown;
  for (let attempt = 0; attempt < 3 && !boundedSignal.aborted; attempt++) {
    await assertTarget();
    if (boundedSignal.aborted) break;
    const stop = page.locator(CHATGPT_STOP_BUTTON_SELECTOR).filter({ visible:true });
    const count = await stop.count();
    const dialogCount = (await visibleStopDialogs(page)).length;
    if (count === 0 && dialogCount === 0) {emit({type:"stop_progress",step:"complete"});return;}
    if (count !== 1 && dialogCount === 0) {
      throw new BrowserCompanionError("ChatGPT does not have one visible Stop control", "selector_drift");
    }
    try {
      if (dialogCount === 0 && await stop.isEnabled()) {
        await stop.click({ timeout:timeout(750), signal:boundedSignal });
        await requested((["initial","retry","recovery"] as const)[attempt]!);
      }
      await waitForStopped(stop);
      emit({type:"stop_progress",step:"complete"});
      return;
    } catch (error) {
      if (signal.aborted || error instanceof BrowserCompanionError) throw error;
      lastError = error;
    }
    if (confirmationSubmitted && (await visibleStopDialogs(page)).length) break;
    if (attempt === 1 && !boundedSignal.aborted) {
      if ((await visibleStopDialogs(page)).length) break;
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
      if ((await composerText(composer)).trim()) {
        await clearExistingDraft(page,boundedSignal);
      }
      try {
        await navigatePage(page,page.url(),{reload:true,timeout:timeout(2_000),signal:boundedSignal});
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

async function waitForConversationComposer(page:Page,signal?:AbortSignal):Promise<void> {
  const throwIfAuthenticationPage=()=>{
    const url=new URL(page.url());
    if(url.origin==="https://auth.openai.com" || url.origin==="https://chatgpt.com" && url.pathname.startsWith("/auth/")) {
      throw new BrowserCompanionError("Sign in to ChatGPT in the browser companion","authentication_required");
    }
  };
  throwIfAuthenticationPage();
  try {
    // DOMContentLoaded precedes React hydration. Missing composer markup at
    // that instant says nothing about the account's authentication state.
    await page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true}).first()
      .waitFor({state:"visible",timeout:10_000,signal});
  } catch(cause) {
    signal?.throwIfAborted();
    await throwIfSessionAttention(page);
    throwIfAuthenticationPage();
    await readChatGptAccount(page);
    throw new BrowserCompanionError("ChatGPT did not expose the conversation's message editor; nothing was sent",
      "editor_unavailable",{cause,phase:"composer_lookup"});
  }
}

async function exactConversationPage(
  surface: SingleBrowserSurface,
  requestedUrl: string,
  expectedUrl: string,
  signal?: AbortSignal,
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
    // Give the current document time to hydrate before spending a recovery
    // load. Persistent missing-editor pages still get one deliberate reload.
    if(!recoveryNeeded){
      try {await waitForConversationComposer(page,signal);}
      catch(error){
        if(!(error instanceof BrowserCompanionError
          &&(error.code==="editor_unavailable"||error.code==="authentication_required"||error.code==="conversation_unavailable")))throw error;
        recoveryNeeded=true;
      }
    }
    if(recoveryNeeded){
      // A known load or sign-in failure needs one deliberate recovery load.
      await navigatePage(page,requestedUrl,{signal,reload:page.url()===requestedUrl});
    }
  }
  await throwIfSessionAttention(page);
  await waitForConversationComposer(page,signal);
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
  private planCache?: {epoch:string;plan:AccountPlan;expiresAt:number};
  private capabilitiesCache?: {
    accountFingerprint:string;
    capabilities:AccountCapabilities;
    catalog:CapabilityCatalog;
  };

  private readonly connectorApps:ConnectorAppCache;
  constructor(private readonly surface: SingleBrowserSurface, private readonly build: string,connectorCachePath?:string) {
    this.connectorApps=new ConnectorAppCache(connectorCachePath);
  }

  async liveness():Promise<boolean> {
    let timeout:ReturnType<typeof setTimeout>|undefined;
    try {
      return await Promise.race([
        this.surface.page().then(page=>page.evaluate(()=>true)),
        new Promise<false>(resolve=>{timeout=setTimeout(()=>resolve(false),1500);}),
      ]);
    }catch{return false;}
    finally{if(timeout)clearTimeout(timeout);}
  }

  async approvePendingConversation(request:ConversationApprovalRequest,signal:AbortSignal):Promise<boolean> {
    const page = await this.surface.page();
    await this.surface.beginAction();
    try {
      return await approveConversationCard(page,request,this.connectorApps.get(request.expectedAccountFingerprint,request.connectorName),signal);
    } finally { await this.surface.endAction(); }
  }

  async health(): Promise<{
    authenticated:boolean;
    accountFingerprint:string | null;
    status:"idle" | "busy" | "authentication_required" | "unavailable";
    detail:string | null;
  }> {
    try {
      const page = await this.surface.page();
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
        account = await this.healthAccount.read(page);
      } catch (error) {
        if (page.isClosed()) throw new BrowserCompanionError("The shared Chrome ChatGPT window was closed", "external_interference", {cause:error});
        if (!await page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true}).count()) {
          throw new BrowserCompanionError(
            "ChatGPT authentication could not be verified in the shared browser window",
            "authentication_required",
            { cause:error },
          );
        }
        throw error;
      }
      try {
        await throwIfSessionAttention(page);
      } catch (error) {
        if (error instanceof BrowserCompanionError && ["conversation_unavailable","conversation_limit"].includes(error.code)) {
          // The account was verified independently. Keep it signed in; the
          // shared navigation budget enforces the conversation-load cooldown.
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
      // A verified account remains available for navigation and maintenance
      // when this conversation has failed to render its composer. Actions
      // still verify the target page and a fresh account before any Send.
      if (!await page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true}).count()) {
        return {authenticated:true,accountFingerprint:account.fingerprint,status:"idle",
          detail:"The current ChatGPT page has no composer; navigation is available"};
      }
      const composer = await activeComposer(page, 2_000).catch(() => null);
      if (!composer) {
        return {
          authenticated:true,
          accountFingerprint:account.fingerprint,
          status:"unavailable",
          detail:"The shared ChatGPT window has no unique usable composer",
        };
      }
      recordConversationAvailable(page);
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
    await this.surface.beginAction();
    try {
      await openNewChat(this.surface, projectUrl,signal);
      await clearExistingDraft(await this.surface.page(),signal);
      return await this.capabilities(signal);
    }
    finally { await this.surface.endAction(); }
  }

  async capabilities(signal?:AbortSignal): Promise<CapabilityCatalog> {
    signal?.throwIfAborted();
    let page = await this.surface.page();
    if (new URL(page.url()).origin !== "https://chatgpt.com") {
      page = await this.surface.navigate("https://chatgpt.com/",signal);
    }
    await throwIfSessionAttention(page);
    try {
      await assertAuthenticatedChatGptPage(page);
    } catch (error) {
      throw new BrowserCompanionError("Sign in to ChatGPT before refreshing browser capabilities", "authentication_required", { cause:error });
    }
    const account = await readChatGptAccount(page);
    const capabilities = await browserStage("capability_discovery","ChatGPT capability discovery failed",()=>detectChatGptAccountCapabilities(page,{signal}));
    const planKind = this.planCache?.plan.accountFingerprint === account.fingerprint
      ? this.planCache.plan.kind : classifyPlan(account.planType).kind;
    const catalog = buildCapabilityCatalog(capabilities, account.fingerprint, this.build, planKind);
    this.capabilitiesCache = { accountFingerprint:account.fingerprint, capabilities, catalog };
    return catalog;
  }

  async accountPlan(epoch:string, signal?:AbortSignal):Promise<AccountPlan> {
    signal?.throwIfAborted();
    const main=await this.surface.page(),account=await readChatGptAccount(main);
    if(this.planCache?.epoch===epoch&&this.planCache.plan.accountFingerprint===account.fingerprint&&Date.now()<this.planCache.expiresAt)return this.planCache.plan;
    let verifiedPlan:AccountPlan|undefined;
    const cache=(plan:AccountPlan,retryAt=0)=>{
      if(!plan.label.trim())throw new BrowserCompanionError("ChatGPT did not expose its subscription plan", "selector_drift");
      this.planCache={epoch,plan,expiresAt:completeAccountPlan(plan)?Infinity:Math.max(Date.now()+60_000,retryAt)};
      return plan;
    };
    try {
      return cache(await this.withMaintenancePage(main,account,async page=>{
        const plan=await readBillingPlan(page,signal,account);
        if(plan.accountFingerprint!==account.fingerprint)throw new BrowserCompanionError("ChatGPT account changed during plan inspection","authentication_required");
        verifiedPlan=plan;return plan;
      },signal,{clearDraft:false}));
    } catch(error) {
      // Returning to the conversation obeys the global loading cooldown. That
      // deferral cannot invalidate subscription data already read and verified.
      if(error instanceof ConversationCooldownError&&verifiedPlan?.label.trim())return cache(verifiedPlan,error.availableAtMs);
      throw error;
    }
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
      const page=await this.surface.page(),account=await readChatGptAccount(page);
      if(expectedAccount&&expectedAccount!==account.fingerprint)throw new BrowserCompanionError("ChatGPT account changed during connector maintenance","authentication_required");
      expectedAccount=account.fingerprint;
      return maintenance?this.withMaintenancePage(page,account,main=>operation(main,account,operationSignal),operationSignal):operation(page,account,operationSignal);
    },signal).catch(error=>{
      const failure=asCompanionError(error);
      throw new BrowserCompanionError(`${label}: ${failure.message}`,failure.code,{cause:error});
    });
    const appId=await step("Identify connector",async(page,account,signal)=>{
      const cached=this.connectorApps.get(account.fingerprint,name);
      if(cached)return cached;
      const discovered=await observedConnectorApp(page,name)??await this.withMaintenancePage(page,account,main=>identifyConnectorApp(main,name,account.fingerprint,signal),signal);
      this.connectorApps.remember(account.fingerprint,name,discovered);return discovered;
    },false);
    if(this.pendingRefresh)throw new BrowserCompanionError("Browser companion is busy with a connector refresh");
    let originalUrl!:string;
    let pending:{page:Page;url:string;account:ChatGptAccount}|undefined;
    let observation:ConnectorRefreshObservation|undefined;
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
    }finally {
      waitSignal.removeEventListener("abort",onAbort);
      // Restore only if the shared page is still ours. Never undo another
      // action's navigation or overwrite a draft typed while refresh waited.
      try {await run(async operationSignal=>{
        const page=await this.surface.page();
        if(!pending||page!==pending.page||page.url()!==pending.url)return;
        await observation?.dispose();
        if(page.url()===originalUrl||await page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true}).count())return;
        await this.surface.beginAction();
        try{await this.surface.navigate(originalUrl,operationSignal);}finally{await this.surface.endAction();}
      });}catch(error){
        // An intervening submission can own the page while observation ends.
        if(!(error instanceof Error&&error.message.toLowerCase().includes("busy")))throw error;
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
      try { await throwIfSessionAttention(page); }
      catch(error) {
        // A page-local load failure must not prevent opening global settings.
        if (!(error instanceof BrowserCompanionError && ["conversation_unavailable","conversation_limit"].includes(error.code))) throw error;
      }
      // Ownership tracks our UI operation, never ChatGPT's response lifetime.
      const composers=page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true});
      const count=await composers.count();
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
        if((await readChatGptAccount(page)).fingerprint!==account)return false;
        if(Date.now()<(this.handoffProbes.get(key)??0))return false;
        this.handoffProbes.delete(key);this.handoffProbes.set(key,Date.now()+180_000);
        if(this.handoffProbes.size>256)this.handoffProbes.delete(this.handoffProbes.keys().next().value!);
        // A bounded visit can discover a final after another chat took the tab.
        // Repeated polls of the current conversation never reload its document.
        page=await this.surface.navigate(url,signal);
      }
      const complete=await completedHandoff(page,url,account,handoffId,signal);
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
    const step = (name:string) => {
      submissionStep = name;
      if (process.env.CODEXIFY_BROWSER_TRACE_SUBMISSION === "1") console.error(`[submission-step] ${execution.request.actionId} ${name}`);
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
      const opened = creating ? await openNewChat(this.surface, execution.request.url === "https://chatgpt.com/new" ? null : execution.request.url, execution.signal) : null;
      const exact = opened ? {page:opened, canonicalUrl:opened.url()} : await exactConversationPage(this.surface, execution.request.url, execution.request.expectedUrl, execution.signal);
      const page = exact.page;
      let canonicalUrl = exact.canonicalUrl;
      restoreUrl = canonicalUrl;
      const account = await readChatGptAccount(page);
      if (account.fingerprint !== execution.request.expectedAccountFingerprint) {
        throw new BrowserCompanionError("ChatGPT account changed before the action could start", "authentication_required");
      }
      ownedPage = page;
      await throwIfConversationApproval(page);
      recordConversationAvailable(page);
      const renewal=execution.request.prompt.kind === "turn_renewal" ? execution.request.prompt : null;
      const verifyHandoff=async()=>{
        if(renewal && !await completedHandoff(page,canonicalUrl,account.fingerprint,renewal.handoffId,execution.signal)) {
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
      if (canStopPrevious) {
        // Offline actions are dispatched after tool-call inactivity. Stop only
        // while preparing this send; ongoing conversations need no observation.
        await stopPrevious();
        if (canonicalChatGptConversationUrl(page.url()) !== canonicalUrl) {
          throw new BrowserCompanionError("ChatGPT changed conversations while stopping the previous response", "conversation_unavailable");
        }
      } else {
        await clearExistingDraft(page,execution.signal);
      }
      const cache = this.capabilitiesCache;
      await prepareBeforeSend(()=>clearExistingDraft(page,execution.signal));
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
      const preferredModel = execution.request.model, preferredEffort = execution.request.effort;
      if (!creating && execution.request.prompt.kind !== "pair_verify"
        && preferredModel !== null && preferredEffort !== null) {
        step("selecting model and effort");
        selected = await prepareBeforeSend(()=>selectAvailableModelAndEffort(
          page,
          preferredModel,
          preferredEffort,
          cache?.accountFingerprint === account.fingerprint ? cache.capabilities : undefined,
          execution.signal,
          verifyFallbackDestination,
        ));
        execution.emit({ type:"model_verified", model:selected.id, effort:selected.effort });
      }
      if (creating || execution.request.prompt.kind === "pair_verify") {
        step("selecting pairing Medium");
        pairingSelection = await selectPairingMedium(page, execution.signal);
      }
      try {
        selectedConnector = await prepareBeforeSend(()=>selectConnector(page, execution.request.connectorName, execution.signal));
      } catch(error) {
        // selectConnector has cleared the unsuccessful mention search. A missing
        // menu row does not require another page load or another submission.
        if (!(error instanceof ConnectorMenuUnavailableError)) throw error;
        execution.signal.throwIfAborted();
        execution.emit({type:"connector_omitted"});
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
      const baseline = await captureSubmissionBaseline(page, execution.renderedPrompt);
      insertionAttempted = true;
      step("inserting prompt");
      try {await prepareBeforeSend(async()=>insertExactPrompt(selectedConnector?.composer ?? await activeComposer(page,30_000,execution.signal), execution.renderedPrompt));}
      catch(error){
        execution.signal.throwIfAborted();
        await throwIfConversationApproval(page);
        if(error instanceof BrowserCompanionError)throw error;
        throw new BrowserCompanionError("ChatGPT did not accept the message in its editor; nothing was sent","editor_unavailable",{cause:error});
      }
      step("waiting for Send control");
      const send = await uniqueSendButton(await activeComposer(page, 30_000, execution.signal), 30_000, execution.signal,
        () => throwIfConversationApproval(page));
      await throwIfConversationApproval(page);
      step("checking model and connector");
      if (pairingSelection) await pairingSelection.verify(execution.signal);
      else if (selected) await assertSelectedModelAndEffort(page, selected, execution.signal, creating);
      if (!await connectorMatches(await activeComposer(page, 30_000, execution.signal))) {
        throw new BrowserCompanionError("ChatGPT changed the selected connector before Send", "external_interference");
      }
      step("checking prompt and conversation");
      if (await composerText(await activeComposer(page, 30_000, execution.signal)) !== execution.renderedPrompt) {
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
      sendActivated = true;
      if(this.pendingSubmission?.actionId===execution.request.actionId)this.pendingSubmission.mayHaveSent=true;
      step("clicking Send");
      submissionSettlesAt = Date.now() + 5_000;
      await send.click({ timeout:10_000, signal:execution.signal });
      submissionSettlesAt = Date.now() + 5_000;
      step("confirming Send acceptance");
      try {
        await waitForSubmissionAcceptance(page, baseline, Date.now() + 30_000, execution.signal);
      } catch (error) {
        if(error instanceof BrowserCompanionError&&error.code==="conversation_limit")throw error;
        throw new BrowserCompanionError(
          "ChatGPT Send may have been accepted, but semantic acceptance could not be proven",
          "ambiguous_send",
          { sendAccepted:true, cause:error },
        );
      }
      sendAccepted = true;
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
    } catch (error) {
      if (process.env.CODEXIFY_BROWSER_TRACE_SUBMISSION === "1") console.error(`[submission-step] ${execution.request.actionId} failed at ${submissionStep}; sendActivated=${sendActivated}; sendAccepted=${sendAccepted}`);
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
      throw error;
    } finally {
      try {
        await settleSubmission();
        if (pairingSelection && ownedPage && restoreUrl) {
          try { await this.restorePairingSelection(ownedPage, pairingSelection, restoreUrl, execution.request.expectedAccountFingerprint); }
          catch (error) {
            throw new BrowserCompanionError("Could not restore the conversation's model and effort after the one-off pairing request", "model_unavailable", {sendAccepted:sendActivated, cause:error});
          }
        }
      } finally {
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
    await this.surface.close();
  }

}
