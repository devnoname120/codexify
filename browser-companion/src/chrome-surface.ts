// Replaces the Electron/CDP surface adapted from miuuyy/codex-chatgpt-web
// launcher/electron/browser-host.cjs at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import { mkdir } from "node:fs/promises";
import { resolve } from "node:path";
import { chromium, type BrowserContext, type Page } from "patchright";
import type { SingleBrowserSurface } from "./chatgpt-driver";
import { BrowserCompanionError } from "./errors";
import { backgroundChromeArgs } from "./background-chrome";
import {prepareRequestBlockList,requestBlockListArgs,verifyRequestBlockList} from "./request-block-list";
import {isChatGptPage,navigatePage,observeConversationRateLimits} from "./navigation";
import {browserSimplicityFromEnvironment,parseBrowserSimplicity,installMinimalChatGptPage,styleRestoredChatGptPage,type BrowserSimplicity} from "./minimal-page";
import {recoverOrphanedWindowsChrome} from "./windows-chrome-recovery";
import {normalizeWindowsRendererPriorities} from "./windows-priority";
import {readPresentationCss,savePresentationCss,validatePresentationCss} from "./presentation-style";
import {createHash} from "node:crypto";
import {maintainRendererMemory} from "./renderer-memory";

const CHATGPT_HOME = "https://chatgpt.com/";

export interface ChromeSurfaceOptions {
  profilePath: string;
  executablePath: string;
  requestBlockList?: readonly string[];
  managedRuntimeRoot?: string;
  presentationCssPath?:string;
  minimalUi?:boolean;
  browserSimplicity?:BrowserSimplicity;
  /** Total owned action pages in this profile, including the primary page. */
  actionLaneLimit?:number;
}

interface SurfaceContextState {
  contextClosed:boolean;
  closed:Promise<void>;
  registeredLanePages:Set<Page>;
  surfaces:Set<ChromeSingleWindowSurface>;
  registration:Promise<void>;
  pendingLaneCreations:number;
  presentationUpdating:boolean;
  presentationCssPath?:string;
  presentationCss:string;
  presentationRevision:number;
  browserSimplicity:BrowserSimplicity;
  actionLaneLimit:number;
  closePromise?:Promise<void>;
}

export class ChromeSingleWindowSurface implements SingleBrowserSurface {
  readonly requireSubmissionNetworkProof=true;
  readonly closed: Promise<void>;
  private actionActive = false;
  private closePromise?:Promise<void>;

  private constructor(private readonly context: BrowserContext, private readonly sharedPage: Page,
    private readonly state:SurfaceContextState, private readonly ownsContext:boolean) {
    state.registeredLanePages.add(sharedPage);
    state.surfaces.add(this);
    const pageClosed = new Promise<void>(resolveClosed => sharedPage.once("close", () => {
      state.registeredLanePages.delete(sharedPage);
      state.surfaces.delete(this);
      resolveClosed();
    }));
    this.closed = ownsContext ? state.closed : Promise.race([state.closed,pageClosed]);
  }

  static async create(options: ChromeSurfaceOptions): Promise<ChromeSingleWindowSurface> {
    const actionLaneLimit=options.actionLaneLimit??2;
    if(!Number.isInteger(actionLaneLimit)||actionLaneLimit<1||actionLaneLimit>4)
      throw new Error("Browser action lane limit must be between 1 and 4");
    const profilePath = resolve(options.profilePath);
    await mkdir(profilePath, { recursive:true, mode:0o700 });
    await recoverOrphanedWindowsChrome({...options, profilePath}).catch(() => {
      // A failed WMI probe must not prevent a normal browser from starting.
      // If an orphan still owns the profile, Chrome's own lock fails closed.
      console.warn("Could not inspect private Chrome startup recovery");
    });
    const policy=await prepareRequestBlockList(profilePath,options.requestBlockList||[]);
    const browserSimplicity=options.browserSimplicity !== undefined ? parseBrowserSimplicity(options.browserSimplicity)
      : options.minimalUi !== undefined ? options.minimalUi ? "simplified" : "off" : browserSimplicityFromEnvironment();
    const minimalUi=browserSimplicity !== "off";
    const presentationCss=minimalUi?await readPresentationCss(options.presentationCssPath):"";
    const context = await chromium.launchPersistentContext(profilePath, {
      channel:"chrome",
      executablePath:resolve(options.executablePath),
      headless:false,
      viewport:null,
      ignoreDefaultArgs:true,
      timeout:30_000,
      ...(minimalUi ? {reducedMotion:"reduce" as const} : {}),
      args:requestBlockListArgs(await backgroundChromeArgs(profilePath),policy),
      // Keep Chrome's own browser headers and user agent.
    });
    try {
      await normalizeWindowsRendererPriorities(context,options);
      if (minimalUi) {
        await installMinimalChatGptPage(context,presentationCss,0,browserSimplicity);
      }
      await verifyRequestBlockList(context,policy);
      let page = context.pages().find(page => isChatGptPage(page.url())) ?? context.pages()[0];
      if (!page) {
        const browser = context.browser();
        if (!browser) throw new Error("Persistent Chrome did not expose its browser connection");
        const session = await browser.newBrowserCDPSession();
        try {
          [page] = await Promise.all([
            context.waitForEvent("page"),
            session.send("Target.createTarget", { url:"about:blank", newWindow:true, background:true }),
          ]);
        } finally {
          await session.detach();
        }
      }
      // A dedicated profile can restore tabs from an interrupted previous run.
      // Only explicitly created lanes may become additional action targets.
      for (const restored of context.pages()) {
        if (restored !== page) await restored.close();
      }
      let resolveClosed!:()=>void;
      const state:SurfaceContextState={contextClosed:false,closed:new Promise(resolve=>{resolveClosed=resolve;}),
        registeredLanePages:new Set(),surfaces:new Set(),registration:Promise.resolve(),pendingLaneCreations:0,
        presentationUpdating:false,presentationCssPath:options.presentationCssPath,presentationCss,presentationRevision:0,
        browserSimplicity,actionLaneLimit};
      const closed=()=>{state.contextClosed=true;resolveClosed();};
      context.once("close",closed);
      const pageClosed=()=>{
        // Closing the last window can leave Chrome's background process alive.
        if(context.pages().every(page=>page.isClosed()))closed();
      };
      page.once("close",pageClosed);
      context.on("page",page=>page.once("close",pageClosed));
      const surface = new ChromeSingleWindowSurface(context,page,state,true);
      observeConversationRateLimits(page);
      if(!isChatGptPage(page.url())) {
        try {
          await surface.navigate(CHATGPT_HOME);
        } catch (error) {
          // Chrome may restore/navigate its startup tab concurrently with the
          // first goto. That navigation abort is not a browser crash. Recover
          // once, before publishing the control server or accepting any Send.
          if (!(error instanceof Error) || !/net::ERR_ABORTED|frame was detached/i.test(error.message)
            || page.isClosed()) throw error;
          if (isChatGptPage(page.url())) {
            await page.waitForLoadState("domcontentloaded", {timeout:30_000});
            if (!isChatGptPage(page.url())) throw error;
          } else if (page.url() === "about:blank") {
            await surface.navigate(CHATGPT_HOME);
          } else throw error;
        }
      }
      else if (minimalUi) await styleRestoredChatGptPage(page,presentationCss,0,browserSimplicity);
      return surface;
    } catch (error) {
      await context.close().catch(() => {});
      throw error;
    }
  }

  async page(): Promise<Page> {
    // newPage emits its page event before returning the exact new handle. Wait
    // for registration so another lane does not mistake that gap for a popup.
    await this.state.registration;
    return this.ownedPage();
  }

  private ownedPage():Page {
    if (this.state.contextClosed || this.sharedPage.isClosed()) {
      throw new BrowserCompanionError("The shared Chrome ChatGPT window was closed", "external_interference");
    }
    const pages = this.context.pages().filter(page => !page.isClosed());
    if (!pages.includes(this.sharedPage)||pages.some(page=>!this.state.registeredLanePages.has(page))) {
      throw new BrowserCompanionError("Chrome exposed an unmanaged browser page outside the action lanes", "external_interference");
    }
    return this.sharedPage;
  }

  async createActionLane():Promise<ChromeSingleWindowSurface> {
    if(!this.ownsContext)throw new Error("Only the primary browser surface can create action lanes");
    if(this.state.presentationUpdating)throw new BrowserCompanionError("Browser presentation is being updated");
    const previous=this.state.registration;
    let release!:()=>void;
    this.state.registration=new Promise(resolve=>{release=resolve;});
    this.state.pendingLaneCreations++;
    let page:Page|undefined;
    try {
      await previous;
      this.ownedPage();
      if(this.state.registeredLanePages.size>=this.state.actionLaneLimit)
        throw new BrowserCompanionError("Browser action lane capacity is already allocated");
      page=await this.context.newPage();
      const lane=new ChromeSingleWindowSurface(this.context,page,this.state,false);
      observeConversationRateLimits(page);
      release();
      await lane.navigate(CHATGPT_HOME);
      if(this.state.browserSimplicity!=="off")
        await styleRestoredChatGptPage(page,this.state.presentationCss,this.state.presentationRevision,this.state.browserSimplicity);
      return lane;
    }catch(error){
      if(page&&!page.isClosed())await page.close().catch(()=>{});
      throw error;
    }finally{release();this.state.pendingLaneCreations--;}
  }

  async setPresentationStyle(css:string,signal?:AbortSignal) {
    signal?.throwIfAborted();validatePresentationCss(css);
    const state=this.state;
    if(state.presentationUpdating||state.pendingLaneCreations||[...state.surfaces].some(surface=>surface.actionActive))
      throw new BrowserCompanionError("Browser companion is busy with an action");
    if(!state.presentationCssPath||state.browserSimplicity === "off")throw new Error("Enable browser simplifications in settings before changing fast presentation");
    state.presentationUpdating=true;
    try {
      const page=await this.page();
      const pages=[...state.registeredLanePages];
      if(pages.some(page=>!isChatGptPage(page.url())))throw new Error("Fast presentation requires the ChatGPT page");
      await page.evaluate(value=>{const sheet=new CSSStyleSheet();sheet.replaceSync(value);if(value.trim()&&!sheet.cssRules.length)throw new Error("No valid CSS rules");},css);
      signal?.throwIfAborted();
      if(css!==state.presentationCss){
        await savePresentationCss(state.presentationCssPath,css);
        state.presentationCss=css;state.presentationRevision++;
        // Revision checks make init-script ordering irrelevant on later loads.
        await installMinimalChatGptPage(this.context,css,state.presentationRevision,state.browserSimplicity);
      }
      await Promise.all(pages.map(page=>styleRestoredChatGptPage(page,css,state.presentationRevision,state.browserSimplicity)));
      return {applied:true,revision:state.presentationRevision,sha256:createHash("sha256").update(css).digest("hex")};
    }finally{state.presentationUpdating=false;}
  }

  async navigate(url: string, signal?: AbortSignal): Promise<Page> {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    const target = new URL(url);
    if (target.protocol !== "https:") throw new Error("Browser window navigation requires HTTPS");
    const page = await this.page();
    await navigatePage(page,target.href,{signal});
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    return page;
  }

  async reveal(): Promise<void> {
    const page = await this.page();
    const session = await this.context.newCDPSession(page);
    try {
      const window = await session.send("Browser.getWindowForTarget");
      if (window.bounds.windowState === "minimized") {
        await session.send("Browser.setWindowBounds", { windowId:window.windowId, bounds:{ windowState:"normal" } });
      }
    } finally {
      await session.detach();
    }
    await page.bringToFront();
  }

  async beginAction(): Promise<void> {
    if (this.actionActive) throw new Error("The shared browser window already owns an action");
    if(this.state.presentationUpdating)throw new BrowserCompanionError("Browser presentation is being updated");
    this.actionActive = true;
    try {await this.page();}catch(error){this.actionActive=false;throw error;}
  }

  async endAction(): Promise<void> {
    // Keep the action lease through occasional cleanup, never during Send or
    // response observation. Low-memory pages incur one small read per minute.
    try {await maintainRendererMemory(await this.page());}
    catch { /* A page closing during cleanup preserves the action result. */ }
    finally {this.actionActive = false;}
  }

  /** Retire one failed action page without terminating healthy sibling lanes. */
  async closeActionPage():Promise<void> {
    this.closePromise??=this.sharedPage.isClosed()?Promise.resolve():this.sharedPage.close();
    await this.closePromise;
  }

  async close(): Promise<void> {
    if(this.ownsContext){
      this.state.closePromise??=this.context.close();
      await this.state.closePromise;
    }else{
      await this.closeActionPage();
    }
  }
}
