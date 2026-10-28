// Replaces the Electron/CDP surface adapted from miuuyy/codex-chatgpt-web
// launcher/electron/browser-host.cjs at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import { mkdir } from "node:fs/promises";
import { resolve } from "node:path";
import { chromium, type BrowserContext, type Page } from "patchright";
import type { SingleBrowserSurface } from "./chatgpt-driver";
import { BrowserCompanionError } from "./errors";
import { backgroundChromeArgs } from "./background-chrome";
import {prepareRequestBlockList,requestBlockListArgs,verifyRequestBlockList} from "./request-block-list";
import {isChatGptPage,navigatePage} from "./navigation";
import {installChatModeCompatibility} from "./chat-mode";

const CHATGPT_HOME = "https://chatgpt.com/";

export interface ChromeSurfaceOptions {
  profilePath: string;
  executablePath: string;
  requestBlockList?: readonly string[];
}

export class ChromeSingleWindowSurface implements SingleBrowserSurface {
  readonly closed: Promise<void>;
  private contextClosed = false;
  private actionActive = false;

  private constructor(private readonly context: BrowserContext, private readonly sharedPage: Page) {
    this.closed = new Promise(resolveClosed => {
      const closed = () => { this.contextClosed = true; resolveClosed(); };
      context.once("close", closed);
      const pageClosed = () => {
        // Closing the last window can leave Chrome's background process alive.
        // Let the launcher exit so the next action can start a fresh window.
        if (context.pages().every(page => page.isClosed())) closed();
      };
      sharedPage.once("close", pageClosed);
      context.on("page", page => page.once("close", pageClosed));
    });
  }

  static async create(options: ChromeSurfaceOptions): Promise<ChromeSingleWindowSurface> {
    const profilePath = resolve(options.profilePath);
    await mkdir(profilePath, { recursive:true, mode:0o700 });
    const policy=await prepareRequestBlockList(profilePath,options.requestBlockList||[]);
    const context = await chromium.launchPersistentContext(profilePath, {
      channel:"chrome",
      executablePath:resolve(options.executablePath),
      headless:false,
      viewport:null,
      ignoreDefaultArgs:true,
      args:requestBlockListArgs(await backgroundChromeArgs(profilePath),policy),
      // Keep Chrome's own browser headers and user agent.
    });
    try {
      await context.addInitScript(installChatModeCompatibility);
      await verifyRequestBlockList(context,policy);
      let page = context.pages()[0];
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
      // Keep one shared page rather than allowing multiple action targets.
      for (const restored of context.pages()) {
        if (restored !== page) await restored.close();
      }
      const surface = new ChromeSingleWindowSurface(context, page);
      if(!isChatGptPage(page.url()))await surface.navigate(CHATGPT_HOME);
      else await page.evaluate(installChatModeCompatibility, undefined, undefined, false);
      return surface;
    } catch (error) {
      await context.close().catch(() => {});
      throw error;
    }
  }

  async page(): Promise<Page> {
    if (this.contextClosed || this.sharedPage.isClosed()) {
      throw new BrowserCompanionError("The shared Chrome ChatGPT window was closed", "external_interference");
    }
    const pages = this.context.pages().filter(page => !page.isClosed());
    if (pages.length !== 1 || pages[0] !== this.sharedPage) {
      throw new BrowserCompanionError(`Chrome exposed ${pages.length} browser pages for one shared window`, "external_interference");
    }
    return this.sharedPage;
  }

  async navigate(url: string, signal?: AbortSignal): Promise<Page> {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    const target = new URL(url);
    if (target.protocol !== "https:") throw new Error("Browser window navigation requires HTTPS");
    const page = await this.page();
    await navigatePage(page,target.href,{signal});
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    await page.waitForLoadState("domcontentloaded", { timeout:30_000,signal }).catch(error => { if(signal?.aborted)throw error; });
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
    await this.page();
    this.actionActive = true;
  }

  async endAction(): Promise<void> {
    this.actionActive = false;
  }

  async close(): Promise<void> {
    await this.context.close();
  }
}
