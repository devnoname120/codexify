import { mkdirSync } from "node:fs";
import { writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { app } from "electron";
import { SingleWindowCoordinator } from "./browser-window";
import {PooledWindowCoordinator} from "./browser-pool";
import { ChatGptBrowserDriver } from "./chatgpt-driver";
import { startControlServer, type ControlServer } from "./control-server";
import { ChromeSingleWindowSurface } from "./chrome-surface";
import { privateChromeExecutable } from "./chrome-executable";
import { PROTOCOL_VERSION, protocolBuild, validateProtocolBuild } from "./protocol";
import {parseRequestBlockList} from "./request-block-list";
import {configureNavigationTimings,configureNavigationPersistence} from "./navigation";
import {shutdownWithDeadline} from "./shutdown";
import {startAutoApprovalMonitor} from './approval-monitor';

if (process.platform === "darwin") app.setActivationPolicy("prohibited");

const BUILD = process.env.CODEXIFY_BROWSER_BUILD
  ? validateProtocolBuild(process.env.CODEXIFY_BROWSER_BUILD)
  : protocolBuild("1.0.0");
const root = resolve(process.env.CODEXIFY_BROWSER_HOME ?? join(homedir(), ".codexify", "browser"));
const profilePath = join(root, "chrome-profile");
const descriptorPath = resolve(process.env.CODEXIFY_BROWSER_DESCRIPTOR ?? join(root, "descriptor.json"));
const smokeFile = process.env.CODEXIFY_BROWSER_SMOKE_FILE;

let server: ControlServer | undefined;
let surface: ChromeSingleWindowSurface | undefined;
let stopping: Promise<void> | undefined;

async function stop(): Promise<void> {
  stopping ??= (async () => {
    console.error(`[browser-lifecycle] at=${new Date().toISOString()} pid=${process.pid} shutdown_requested`);
    await shutdownWithDeadline(async () => {
      await (server ? server.close() : surface?.close())?.catch(error => {
        console.error(`Browser companion shutdown failed: ${error instanceof Error ? error.message : String(error)}`);
      });
    }, () => {
      console.error(`[browser-lifecycle] at=${new Date().toISOString()} pid=${process.pid} shutdown_deadline_exit`);
      app.exit(0);
    });
    server = undefined;
    surface = undefined;
  })();
  await stopping;
}

async function main(): Promise<void> {
  const launcherPath = join(root, "launcher");
  mkdirSync(launcherPath, { recursive:true, mode:0o700 });
  app.setPath("userData", launcherPath);
  if (!app.requestSingleInstanceLock()) {
    app.quit();
    return;
  }
  await app.whenReady();
  const browserRoot = app.isPackaged
    ? join(process.resourcesPath, "chrome")
    : resolve(import.meta.dirname, "../.browsers", `${process.platform}-${process.arch}`);
  const executablePath = await privateChromeExecutable(browserRoot);
  const requestBlockList=parseRequestBlockList(process.env.CODEXIFY_BROWSER_REQUEST_BLOCK_LIST);
  configureNavigationTimings(process.env.CODEXIFY_BROWSER_NAVIGATION_TIMINGS);
  configureNavigationPersistence(join(root,"navigation-cooldown.json"));
  const actionLaneLimit=Number(process.env.CODEXIFY_BROWSER_ACTION_LANES??2);
  if(!Number.isInteger(actionLaneLimit)||actionLaneLimit<1||actionLaneLimit>4)throw new Error("Browser action pages must be between one and four");
  surface = await ChromeSingleWindowSurface.create({ profilePath, executablePath, requestBlockList,
    actionLaneLimit,
    presentationCssPath:join(root,"fast-ui.css"),
    managedRuntimeRoot:app.isPackaged ? join(root,"runtime") : undefined });
  const driver = new ChatGptBrowserDriver(surface, BUILD,join(root,"connector-apps.json"));
  const lanes=[new SingleWindowCoordinator(driver, BUILD)];
  for(let index=1;index<actionLaneLimit;index++) {
    const lane=await surface.createActionLane();
    lanes.push(new SingleWindowCoordinator(driver.fork(lane),BUILD));
  }
  const coordinator = new PooledWindowCoordinator(lanes,BUILD,()=>surface!.close());
  server = await startControlServer(coordinator, descriptorPath);
  startAutoApprovalMonitor(coordinator,surface,root);
  app.on("before-quit", event => {
    if (stopping) return;
    event.preventDefault();
    void stop().finally(() => app.exit(0));
  });
  console.error(`[browser-lifecycle] at=${new Date().toISOString()} pid=${process.pid} renderer_ready`);
  void surface.closed.then(() => { if (!stopping) {console.error(`[browser-lifecycle] at=${new Date().toISOString()} pid=${process.pid} renderer_context_closed`);app.quit();} });
  process.once("SIGINT", () => { void stop().finally(() => app.exit(0)); });
  process.once("SIGTERM", () => { void stop().finally(() => app.exit(0)); });
}

async function smoke(): Promise<void> {
  if (!smokeFile) throw new Error("Browser companion smoke marker path is missing");
  await app.whenReady();
  await writeFile(resolve(smokeFile), `${JSON.stringify({
    ok:true,
    packaged:app.isPackaged,
    protocolVersion:PROTOCOL_VERSION,
    build:BUILD,
    platform:process.platform,
    arch:process.arch,
  })}\n`, { mode:0o600 });
  app.exit(0);
}

void (smokeFile ? smoke() : main()).catch(error => {
  console.error(`Browser companion failed: ${error instanceof Error ? error.message : String(error)}`);
  void stop().finally(() => app.exit(1));
});
