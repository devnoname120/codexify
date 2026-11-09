import { expect, test } from "bun:test";
import {
  MAX_DELTA_BYTES,
  MAX_EVENT_LINE_BYTES,
  MAX_EVENT_STREAM_BYTES,
  type ActionEvent,
  type CapabilityCatalog,
  type ExecuteActionRequest,
} from "../src/protocol";
import {
  BROWSER_OPERATION_TIMEOUT_MS,
  MAX_RETAINED_ACTIONS,
  SingleWindowCoordinator,
  type BrowserDriver,
  type DriverExecution,
} from "../src/browser-window";
import { protocolBuild } from "../src/protocol";
import { BrowserCompanionError, ConversationCooldownError } from "../src/errors";
import {navigatePage} from "../src/navigation";
import type {BrowserContext,Page} from "patchright";

const TEST_BUILD = protocolBuild("test-build");

const request = (id: string): ExecuteActionRequest => ({
  actionId:id,
  idempotencyKey:`idem-${id}`,
  url:`https://chatgpt.com/c/${id}`,
  pairingGeneration:1,
  connectorName:"Codexify",
  expectedAccountFingerprint:"account-1",
  model:"gpt-6-sol",
  effort:"high",
  prompt:{ kind:"offline_chat", batchId:`batch-${id}`, messages:[{ id:`message-${id}`, markdown:"Continue." }] },
  expectedUrl:`https://chatgpt.com/c/${id}`,
  autoApproveToolCalls:false,
});

class DeferredDriver implements BrowserDriver {
  runs: string[] = [];
  releases = new Map<string, () => void>();
  authenticated = true;
  loginCalls = 0;
  capabilityCalls = 0;
  revealCalls: Array<string | null> = [];
  status: "idle" | "busy" | "authentication_required" | "unavailable" = "idle";

  async health() {
    return {
      authenticated:this.authenticated,
      accountFingerprint:this.authenticated ? "account-1" : null,
      status:this.status,
      detail:this.status === "idle" ? null : "The visible ChatGPT window is not idle",
    };
  }
  async login() { this.loginCalls += 1; this.authenticated = true; return this.health(); }
  async capabilities(): Promise<CapabilityCatalog> {
    this.capabilityCalls += 1;
    return {
      protocolVersion:1, build:TEST_BUILD, accountFingerprint:"account-1", catalogRevision:"catalog-1",
      models:[{ id:"gpt-6-sol", label:"GPT-6 Sol", efforts:[{ id:"high", label:"High", available:true, reason:null }] }],
    };
  }
  async reveal(url: string | null) { this.revealCalls.push(url); }
  async execute(execution: DriverExecution): Promise<void> {
    this.runs.push(execution.request.actionId);
    execution.emit({ type:"navigating" });
    execution.emit({ type:"composer_ready" });
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({ type:"answer_delta", markdown:`answer-${execution.request.actionId}`, continuation:false });
    await new Promise<void>((resolve, reject) => {
      this.releases.set(execution.request.actionId, resolve);
      execution.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once:true });
    });
    execution.emit({ type:"generation_complete", finalMarkdown:`answer-${execution.request.actionId}`, canonicalUrl:execution.request.expectedUrl });
  }
}

class SlowHealthDriver extends DeferredDriver {
  healthCalls = 0;
  activeHealthCalls = 0;
  maxActiveHealthCalls = 0;

  override async health() {
    this.healthCalls += 1;
    this.activeHealthCalls += 1;
    this.maxActiveHealthCalls = Math.max(this.maxActiveHealthCalls, this.activeHealthCalls);
    await Bun.sleep(25);
    this.activeHealthCalls -= 1;
    return super.health();
  }
}

class OversizedDeltaDriver extends DeferredDriver {
  override async execute(execution: DriverExecution): Promise<void> {
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({
      type:"answer_delta",
      markdown:"x".repeat(MAX_DELTA_BYTES + 1),
      continuation:false,
    });
    execution.emit({
      type:"generation_complete",
      finalMarkdown:"Done",
      canonicalUrl:execution.request.expectedUrl,
    });
  }
}

class LargeFinalDriver extends DeferredDriver {
  override async execute(execution: DriverExecution): Promise<void> {
    const chunks = ["a".repeat(60 * 1024), "b".repeat(60 * 1024), "c".repeat(60 * 1024)];
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    for (const [index, markdown] of chunks.entries()) {
      execution.emit({ type:"answer_delta", markdown, continuation:index > 0 });
    }
    execution.emit({
      type:"generation_complete",
      finalMarkdown:chunks.join(""),
      canonicalUrl:execution.request.expectedUrl,
    });
  }
}

class EventFloodDriver extends DeferredDriver {
  override async execute(execution: DriverExecution): Promise<void> {
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    for (let index = 0; index < 300; index += 1) {
      execution.emit({
        type:"status_delta",
        text:`${index.toString().padStart(3, "0")}${"x".repeat(MAX_DELTA_BYTES - 3)}`,
        continuation:false,
      });
    }
  }
}

class PrematureCompletionDriver extends DeferredDriver {
  override async execute(execution: DriverExecution): Promise<void> {
    execution.emit({
      type:"generation_complete",
      finalMarkdown:"Unaccepted output",
      canonicalUrl:execution.request.expectedUrl,
    });
  }
}

class LingeringTerminalDriver extends DeferredDriver {
  private resolveTerminalEmitted!: () => void;
  readonly terminalEmitted = new Promise<void>(resolve => { this.resolveTerminalEmitted = resolve; });
  private resolveReleaseAfterTerminal!: () => void;
  readonly releaseAfterTerminal = new Promise<void>(resolve => { this.resolveReleaseAfterTerminal = resolve; });

  release(): void {
    this.resolveReleaseAfterTerminal();
  }

  override async execute(execution: DriverExecution): Promise<void> {
    this.runs.push(execution.request.actionId);
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({
      type:"generation_complete",
      finalMarkdown:"Done",
      canonicalUrl:execution.request.expectedUrl,
    });
    this.resolveTerminalEmitted();
    await this.releaseAfterTerminal;
  }
}

class AbortableLingeringTerminalDriver extends DeferredDriver {
  private resolveTerminalEmitted!: () => void;
  private resolveRelease!: () => void;
  readonly terminalEmitted = new Promise<void>(resolve => { this.resolveTerminalEmitted = resolve; });
  readonly released = new Promise<void>(resolve => { this.resolveRelease = resolve; });
  aborted = false;

  release() {
    this.resolveRelease();
  }

  override async execute(execution: DriverExecution): Promise<void> {
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({
      type:"generation_complete",
      finalMarkdown:"done",
      canonicalUrl:execution.request.expectedUrl,
    });
    this.resolveTerminalEmitted();
    execution.signal.addEventListener("abort", () => {
      this.aborted = true;
      this.resolveRelease();
    }, { once:true });
    await this.released;
  }
}

class OversizedFailureDriver extends DeferredDriver {
  override async execute(): Promise<void> {
    throw new BrowserCompanionError("x".repeat(MAX_DELTA_BYTES + 1), "selector_drift");
  }
}

class NulFailureDriver extends DeferredDriver {
  override async execute(): Promise<void> {
    throw new BrowserCompanionError("selector\0failure", "selector_drift");
  }
}

class EmptyFailureDriver extends DeferredDriver {
  override async execute(): Promise<void> {
    throw new BrowserCompanionError("", "selector_drift");
  }
}

class JsonExpandingFailureDriver extends DeferredDriver {
  override async execute(): Promise<void> {
    throw new BrowserCompanionError("\\\u0001".repeat(MAX_DELTA_BYTES / 2), "selector_drift");
  }
}

class JsonExpandingAnswerDriver extends DeferredDriver {
  readonly markdown = "\\".repeat(MAX_DELTA_BYTES);

  override async execute(execution: DriverExecution): Promise<void> {
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({ type:"answer_delta", markdown:this.markdown, continuation:false });
    execution.emit({
      type:"generation_complete",
      finalMarkdown:this.markdown,
      canonicalUrl:execution.request.expectedUrl,
    });
  }
}

class ApprovalDriver extends DeferredDriver {
  override async execute(execution: DriverExecution): Promise<void> {
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({ type:"approval_required", message:"Approve once" });
  }
}

async function collect(coordinator: SingleWindowCoordinator, actionId: string): Promise<ActionEvent[]> {
  const events: ActionEvent[] = [];
  for await (const event of coordinator.events(actionId, 0)) events.push(event);
  return events;
}

test("only an explicitly manual action bypasses navigation cooldown and still requires its verified account",async()=>{
  let current="https://chatgpt.com/";const loads:string[]=[],context={} as BrowserContext;
  const page={context:()=>context,url:()=>current,goto:async(url:string)=>{current=url;loads.push(url);return {status:()=>200};}} as unknown as Page;
  await navigatePage(page,"https://chatgpt.com/c/existing");
  class NavigationDriver extends DeferredDriver {
    override async execute(execution:DriverExecution):Promise<void>{
      execution.emit({type:"navigating"});await navigatePage(page,execution.request.url);
      execution.emit({type:"composer_ready"});execution.emit({type:"model_verified",model:"gpt-6-sol",effort:"high"});
      execution.emit({type:"connector_verified"});execution.emit({type:"send_accepted"});
      execution.emit({type:"generation_complete",finalMarkdown:"Done",canonicalUrl:execution.request.url});
    }
  }
  const coordinator=new SingleWindowCoordinator(new NavigationDriver(),TEST_BUILD);
  const automatic=request("cooldown-automatic");await coordinator.execute(automatic);
  const waitingEvents=await collect(coordinator,automatic.actionId);
  const waitingEvent=waitingEvents.at(-1);
  expect(waitingEvent).toMatchObject({type:"waiting",availableAtMs:expect.any(Number)});
  expect(waitingEvents.some(event=>["failed","rate_limited","send_accepted"].includes(event.type))).toBe(false);
  expect(await coordinator.action(automatic.actionId)).toMatchObject({state:"waiting",sendAccepted:false,attention:null,cooldownUntilMs:waitingEvent?.type==="waiting"?waitingEvent.availableAtMs:0});
  expect((await coordinator.health()).activeActionId).toBeNull();
  await coordinator.execute({...automatic,bypassConversationCooldown:false});
  await expect(coordinator.execute({...automatic,bypassConversationCooldown:true})).rejects.toThrow("different request");
  const manual={...request("cooldown-manual"),bypassConversationCooldown:true};await coordinator.execute(manual);
  expect((await collect(coordinator,manual.actionId)).some(event=>event.type==="send_accepted")).toBe(true);
  expect(loads).toEqual(["https://chatgpt.com/c/existing",manual.url]);
  const after=request("cooldown-after");await coordinator.execute(after);
  expect((await collect(coordinator,after.actionId)).some(event=>event.type==="waiting")).toBe(true);
  await coordinator.execute({...request("cooldown-wrong-account"),expectedAccountFingerprint:"other",bypassConversationCooldown:true});
  expect((await collect(coordinator,"cooldown-wrong-account")).at(-1)).toMatchObject({type:"failed",message:expect.stringContaining("account changed")});
  expect(loads).toHaveLength(2);
});

test("one companion window refuses a second task while its submission operation is unsettled", async () => {
  const driver = new DeferredDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const first = await coordinator.execute(request("first"));
  expect(first.state).toBe("queued");
  const eventsPromise = collect(coordinator, "first");
  await Bun.sleep(10);
  expect(driver.runs).toEqual(["first"]);
  await expect(coordinator.execute(request("second"))).rejects.toThrow("busy");
  expect(driver.runs).toEqual(["first"]);
  driver.releases.get("first")?.();
  const events = await eventsPromise;
  expect(events.map(event => event.type)).toEqual([
    "queued", "navigating", "composer_ready", "model_verified", "connector_verified",
    "send_accepted", "answer_delta", "generation_complete",
  ]);
  expect((await coordinator.action("first"))?.state).toBe("idle");
});

test("concurrent execute preflights cannot acquire the single window twice", async () => {
  const driver = new SlowHealthDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const results = await Promise.allSettled([
    coordinator.execute(request("concurrent-first")),
    coordinator.execute(request("concurrent-second")),
  ]);
  try {
    expect(results.filter(result => result.status === "fulfilled")).toHaveLength(1);
    expect(results.filter(result => result.status === "rejected")).toHaveLength(1);
    while (!driver.runs.length) await Bun.sleep(1);
    expect(driver.maxActiveHealthCalls).toBe(1);
    expect(driver.runs).toHaveLength(1);
  } finally {
    await coordinator.close();
  }
});

test("execute is idempotent but an action ID cannot be rebound to another request", async () => {
  const driver = new DeferredDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const submitted = request("same");
  expect((await coordinator.execute(submitted)).actionId).toBe("same");
  expect((await coordinator.execute(structuredClone(submitted))).actionId).toBe("same");
  await expect(coordinator.execute({ ...submitted, effort:"medium" })).rejects.toThrow("different request");
  while (!driver.releases.has("same")) await Bun.sleep(1);
  driver.releases.get("same")?.();
  await collect(coordinator, "same");
  expect(driver.runs).toEqual(["same"]);
});

test("cancellation is idempotent and never starts another action", async () => {
  const driver = new DeferredDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  await coordinator.execute(request("cancel"));
  await Bun.sleep(10);
  const first = await coordinator.cancel({ actionId:"cancel", idempotencyKey:"cancel-1" });
  expect(first.cancelled).toBeTrue();
  expect(first.state).toBe("ambiguous");
  const second = await coordinator.cancel({ actionId:"cancel", idempotencyKey:"cancel-1" });
  expect(second).toEqual(first);
  const events = await collect(coordinator, "cancel");
  expect(events.at(-1)?.type).toBe("ambiguous_send");
  expect(driver.runs).toEqual(["cancel"]);
});

test("a manually busy shared window produces a retained failure before navigation", async () => {
  const driver = new DeferredDriver();
  driver.status = "busy";
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);

  await coordinator.execute(request("manual-busy"));
  expect((await collect(coordinator,"manual-busy")).at(-1)).toMatchObject({type:"failed",message:expect.stringContaining("not idle")});
  expect(driver.runs).toEqual([]);
  expect(await coordinator.action("manual-busy")).toMatchObject({state:"failed",sendAccepted:false});
  expect((await coordinator.health()).status).toBe("busy");
});

test("login cannot navigate the shared window away from an active action", async () => {
  const driver = new DeferredDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  await coordinator.execute(request("active-login"));
  await Bun.sleep(10);

  await expect(coordinator.login()).rejects.toThrow("busy");

  expect(driver.loginCalls).toBe(0);
  expect((await coordinator.health()).activeActionId).toBe("active-login");
  driver.releases.get("active-login")?.();
  await collect(coordinator, "active-login");
});

test("login cannot navigate away from a manually busy shared window", async () => {
  const driver = new DeferredDriver();
  driver.status = "busy";
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);

  await expect(coordinator.login()).rejects.toThrow("not idle");

  expect(driver.loginCalls).toBe(0);
});

test("reveal shows a manually busy shared window without navigating it", async () => {
  const driver = new DeferredDriver();
  driver.status = "busy";
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);

  await coordinator.reveal("https://chatgpt.com/c/requested");

  expect(driver.revealCalls).toEqual([null]);
});

test("capability refresh cannot mutate a manually busy shared window", async () => {
  const driver = new DeferredDriver();
  driver.status = "busy";
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);

  await expect(coordinator.capabilities()).rejects.toThrow("not idle");

  expect(driver.capabilityCalls).toBe(0);
});

test("invalid driver events fail closed before entering the action stream", async () => {
  const coordinator = new SingleWindowCoordinator(new OversizedDeltaDriver(), TEST_BUILD);
  await coordinator.execute(request("invalid-event"));

  const events = await collect(coordinator, "invalid-event");

  expect(events.some(event => event.type === "answer_delta")).toBeFalse();
  expect(events.at(-1)?.type).toBe("ambiguous_send");
});

test("large final answers stay within the event-line contract after streaming deltas", async () => {
  const coordinator = new SingleWindowCoordinator(new LargeFinalDriver(), TEST_BUILD);
  await coordinator.execute(request("large-final"));

  const events = await collect(coordinator, "large-final");
  const terminal = events.at(-1);

  expect(events.filter(event => event.type === "answer_delta")).toHaveLength(3);
  expect(terminal?.type).toBe("generation_complete");
  if (terminal?.type !== "generation_complete") throw new Error("missing terminal event");
  expect(terminal.finalMarkdown).toBeNull();
  expect(Buffer.byteLength(`${JSON.stringify(terminal)}\n`)).toBeLessThanOrEqual(MAX_EVENT_LINE_BYTES);
});

test("one action retains at most the bounded protocol event stream", async () => {
  const coordinator = new SingleWindowCoordinator(new EventFloodDriver(), TEST_BUILD);
  await coordinator.execute(request("event-budget"));

  const events = await collect(coordinator, "event-budget");
  const bytes = events.reduce(
    (total, event) => total + Buffer.byteLength(`${JSON.stringify(event)}\n`),
    0,
  );

  expect(bytes).toBeLessThanOrEqual(MAX_EVENT_STREAM_BYTES);
  expect(events.at(-1)?.type).toBe("ambiguous_send");
});

test("completed action history is bounded without replaying a retired action", async () => {
  const driver = new DeferredDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  for (let index = 0; index < MAX_RETAINED_ACTIONS + 2; index += 1) {
    const actionId = `history-${index}`;
    await coordinator.execute(request(actionId));
    while (!driver.releases.has(actionId)) await Bun.sleep(1);
    driver.releases.get(actionId)!();
    await collect(coordinator, actionId);
  }

  const retired = await coordinator.action("history-0");
  expect(retired?.state).toBe("idle");
  expect(retired?.lastEventSequence).toBe(0);
  const runs = driver.runs.length;
  expect(await coordinator.execute(request("history-0"))).toEqual(retired!);
  expect(driver.runs).toHaveLength(runs);
  await expect(collect(coordinator, "history-0")).rejects.toThrow("no longer retained");
  expect((await coordinator.action(`history-${MAX_RETAINED_ACTIONS + 1}`))?.lastEventSequence)
    .toBeGreaterThan(0);
});

test("a terminal answer without semantic Send acceptance fails closed", async () => {
  const coordinator = new SingleWindowCoordinator(new PrematureCompletionDriver(), TEST_BUILD);
  await coordinator.execute(request("premature-completion"));

  const events = await collect(coordinator, "premature-completion");

  expect(events.some(event => event.type === "generation_complete")).toBeFalse();
  expect(events.at(-1)?.type).toBe("failed");
  expect((await coordinator.action("premature-completion"))?.sendAccepted).toBeFalse();
});

test("the single window stays owned until the driver returns after its terminal event", async () => {
  const driver = new LingeringTerminalDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  await coordinator.execute(request("lingering-first"));
  const firstEvents = collect(coordinator, "lingering-first");
  await driver.terminalEmitted;

  await expect(coordinator.execute(request("lingering-second"))).rejects.toThrow("busy");
  expect(await Promise.race([
    firstEvents.then(() => "settled"),
    Bun.sleep(20).then(() => "waiting"),
  ])).toBe("waiting");

  driver.release();
  expect((await firstEvents).at(-1)?.type).toBe("generation_complete");
  while ((await coordinator.health()).activeActionId) await Bun.sleep(1);
  expect((await coordinator.execute(request("lingering-second"))).actionId).toBe("lingering-second");
  driver.releases.get("lingering-second")?.();
});

test("an event cursor at the terminal sequence waits for driver cleanup", async () => {
  const driver = new LingeringTerminalDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const submitted = request("terminal-cursor");
  await coordinator.execute(submitted);
  await driver.terminalEmitted;
  const terminalSequence = (await coordinator.action(submitted.actionId))?.lastEventSequence;
  if (!terminalSequence) throw new Error("terminal sequence was not recorded");

  const resumed = (async () => {
    const events: ActionEvent[] = [];
    for await (const event of coordinator.events(submitted.actionId, terminalSequence)) {
      events.push(event);
    }
    return events;
  })();

  expect(await Promise.race([
    resumed.then(() => "settled"),
    Bun.sleep(20).then(() => "waiting"),
  ])).toBe("waiting");
  driver.release();
  expect(await resumed).toEqual([]);
});

test("shutdown aborts driver cleanup even after a terminal event", async () => {
  const driver = new AbortableLingeringTerminalDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  await coordinator.execute(request("terminal-cleanup-shutdown"));
  await driver.terminalEmitted;

  const closing = coordinator.close();
  const outcome = await Promise.race([
    closing.then(() => "closed"),
    Bun.sleep(20).then(() => "waiting"),
  ]);
  if (outcome === "waiting") driver.release();
  await closing;

  expect(outcome).toBe("closed");
  expect(driver.aborted).toBeTrue();
});

test("oversized driver failures become bounded terminal events instead of orphaned actions", async () => {
  const coordinator = new SingleWindowCoordinator(new OversizedFailureDriver(), TEST_BUILD);
  await coordinator.execute(request("oversized-failure"));
  await Bun.sleep(10);

  const snapshot = await coordinator.action("oversized-failure");
  expect(snapshot?.state).toBe("failed");
  const events = await collect(coordinator, "oversized-failure");
  const terminal = events.at(-1);
  expect(terminal?.type).toBe("failed");
  if (terminal?.type !== "failed") throw new Error("missing terminal failure");
  expect(Buffer.byteLength(terminal.message)).toBeLessThanOrEqual(MAX_DELTA_BYTES);
});

test("NUL-bearing driver failures become valid terminal events", async () => {
  const coordinator = new SingleWindowCoordinator(new NulFailureDriver(), TEST_BUILD);
  await coordinator.execute(request("nul-failure"));
  await Bun.sleep(10);

  const events = await collect(coordinator, "nul-failure");
  const terminal = events.at(-1);
  expect(terminal?.type).toBe("failed");
  if (terminal?.type !== "failed") throw new Error("missing terminal failure");
  expect(terminal.message).not.toContain("\0");
});

test("empty driver failures still produce a valid terminal event", async () => {
  const coordinator = new SingleWindowCoordinator(new EmptyFailureDriver(), TEST_BUILD);
  await coordinator.execute(request("empty-failure"));
  await Bun.sleep(10);

  const events = await collect(coordinator, "empty-failure");
  const terminal = events.at(-1);
  expect(terminal?.type).toBe("failed");
  if (terminal?.type !== "failed") throw new Error("missing terminal failure");
  expect(terminal.message).toBe("Browser companion operation failed");
});

test("JSON-expanding driver failures still fit one terminal event line", async () => {
  const coordinator = new SingleWindowCoordinator(new JsonExpandingFailureDriver(), TEST_BUILD);
  await coordinator.execute(request("json-expanding-failure"));
  await Bun.sleep(10);

  const snapshot = await coordinator.action("json-expanding-failure");
  expect(snapshot?.state).toBe("failed");
  const events = await collect(coordinator, "json-expanding-failure");
  const terminal = events.at(-1);
  expect(terminal?.type).toBe("failed");
  expect(Buffer.byteLength(`${JSON.stringify(terminal)}\n`)).toBeLessThanOrEqual(MAX_EVENT_LINE_BYTES);
});

test("JSON-expanding answer deltas are split without changing Markdown", async () => {
  const driver = new JsonExpandingAnswerDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  await coordinator.execute(request("json-expanding-answer"));

  const events = await collect(coordinator, "json-expanding-answer");
  const deltas = events.filter((event): event is Extract<ActionEvent, { type:"answer_delta" }> => (
    event.type === "answer_delta"
  ));

  expect(deltas.length).toBeGreaterThan(1);
  expect(deltas.map(event => event.markdown).join("")).toBe(driver.markdown);
  expect(deltas[0]?.continuation).toBeFalse();
  expect(deltas.slice(1).every(event => event.continuation)).toBeTrue();
  expect(events.at(-1)?.type).toBe("generation_complete");
  expect(events.every(event => (
    Buffer.byteLength(`${JSON.stringify(event)}\n`) <= MAX_EVENT_LINE_BYTES
  ))).toBeTrue();
});

test("post-Send attention snapshots retain the verified conversation URL", async () => {
  const coordinator = new SingleWindowCoordinator(new ApprovalDriver(), TEST_BUILD);
  const submitted = request("approval-after-send");
  await coordinator.execute(submitted);
  await collect(coordinator, submitted.actionId);

  const snapshot = await coordinator.action(submitted.actionId);
  expect(snapshot?.state).toBe("approval_required");
  expect(snapshot?.sendAccepted).toBeTrue();
  expect(snapshot?.canonicalUrl).toBe(submitted.expectedUrl);
});

test("pairing and default conversation actions allow an omitted connector and still require account proof", async () => {
  class ConversationDriver extends DeferredDriver {
    override async execute(execution: DriverExecution) {
      this.runs.push(execution.request.actionId);
      execution.emit({type:"navigating"});execution.emit({type:"composer_ready"});
      execution.emit({type:"send_accepted"});
      execution.emit({type:"generation_complete",finalMarkdown:"Verified.",canonicalUrl:execution.request.expectedUrl});
    }
  }
  const driver = new ConversationDriver();
  const coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  const pair = {...request("no-model-pair"),model:null,effort:null,prompt:{kind:"pair_verify" as const,reference:"A".repeat(43)}};
  await coordinator.execute(pair);
  let snapshot;
  for (let attempt=0;attempt<20;attempt++) {snapshot=await coordinator.action(pair.actionId);if(snapshot?.state==="idle")break;await Bun.sleep(5);}
  expect(snapshot?.sendAccepted).toBeTrue();expect(snapshot?.selected).toBeNull();
  const wrongAccount = {...pair,actionId:"wrong-account-pair",idempotencyKey:"wrong-account-idem",expectedAccountFingerprint:"different-account"};
  await coordinator.execute(wrongAccount);
  expect((await collect(coordinator,wrongAccount.actionId)).at(-1)).toMatchObject({type:"failed",message:expect.stringContaining("account changed")});
  expect(driver.runs).toEqual(["no-model-pair"]);
  await coordinator.close();
});

test('authentication recovery owns the action and rechecks sign-in and the paired account before execution',async()=>{
 for(const outcome of ['restored','signed-out','changed-account'] as const){
  class RecoveryDriver extends DeferredDriver {
   recoveryCalls=0;
   override async health(){const health=await super.health();return {...health,accountFingerprint:this.authenticated&&outcome==='changed-account'?'other-account':health.accountFingerprint};}
   async recoverAuthentication(_request:ExecuteActionRequest,signal:AbortSignal){signal.throwIfAborted();this.recoveryCalls++;this.authenticated=outcome!=='signed-out';this.status=this.authenticated?'idle':'authentication_required';}
   override async execute(execution:DriverExecution){this.runs.push(execution.request.actionId);execution.emit({type:'model_verified',model:execution.request.model!,effort:execution.request.effort!});execution.emit({type:'connector_verified'});execution.emit({type:'send_accepted'});execution.emit({type:'submission_complete',canonicalUrl:execution.request.expectedUrl});}
  }
  const driver=new RecoveryDriver();driver.authenticated=false;driver.status='authentication_required';
  const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD),submitted=request(`recover-${outcome}`);
  await coordinator.execute(submitted);const events=await collect(coordinator,submitted.actionId);
  expect(driver.recoveryCalls).toBe(1);
  expect(events.at(-1)?.type).toBe(outcome==='restored'?'submission_complete':'authentication_required');
  expect(driver.runs).toEqual(outcome==='restored'?[submitted.actionId]:[]);
  expect((await coordinator.action(submitted.actionId))?.sendAccepted).toBe(outcome==='restored');
  expect((await coordinator.health()).activeActionId).toBeNull();
 }
});

test('an older driver without authentication recovery cannot execute an unauthenticated action',async()=>{
 const driver=new DeferredDriver();driver.authenticated=false;driver.status='authentication_required';
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD),submitted=request('missing-recovery');
 await coordinator.execute(submitted);const events=await collect(coordinator,submitted.actionId);
 expect(events.at(-1)?.type).toBe('authentication_required');expect(driver.runs).toEqual([]);
});


test("automatic pairing verifies conversation preferences after acceptance and before completion",async()=>{
  for(const restore of [true,false]){
    class PairingDriver extends DeferredDriver {
      override async execute(execution:DriverExecution):Promise<void>{
        execution.emit({type:"connector_verified"});execution.emit({type:"send_accepted"});
        if(restore)execution.emit({type:"model_verified",model:execution.request.model!,effort:execution.request.effort!});
        execution.emit({type:"submission_complete",canonicalUrl:"https://chatgpt.com/c/created-pairing"});
      }
    }
    const coordinator=new SingleWindowCoordinator(new PairingDriver(),TEST_BUILD);
    const automatic:ExecuteActionRequest={...request(`automatic-${restore}`),url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",prompt:{kind:"create_chat",reference:"A".repeat(43),continuationToken:Buffer.alloc(32,2).toString("base64url"),setupReference:null}};
    await coordinator.execute(automatic);const events=[];for await(const event of coordinator.events(automatic.actionId,0))events.push(event);
    expect(events.some(event=>event.type==="send_accepted")).toBeTrue();
    if(restore){expect(events.at(-1)?.type).toBe("submission_complete");expect((await coordinator.action(automatic.actionId))?.selected).toEqual({model:automatic.model!,effort:automatic.effort!});}
    else{expect(events.at(-1)?.type).toBe("ambiguous_send");const last=events.at(-1);expect(last&&"message" in last?last.message:"").toContain("before restoring its requested model and effort");}
  }
});

test("ordinary sends without a connector still require model verification",async()=>{
  for(const verified of [true,false]) {
    class OptionalConnectorDriver extends DeferredDriver {
      override async execute(execution:DriverExecution):Promise<void>{
        if(verified)execution.emit({type:"model_verified",model:execution.request.model!,effort:execution.request.effort!});
        execution.emit({type:"send_accepted"});
        execution.emit({type:"submission_complete",canonicalUrl:execution.request.expectedUrl});
      }
    }
    const coordinator=new SingleWindowCoordinator(new OptionalConnectorDriver(),TEST_BUILD),ordinary=request(`optional-connector-${verified}`);
    await coordinator.execute(ordinary);const events=await collect(coordinator,ordinary.actionId);
    expect(events.at(-1)?.type).toBe(verified?"submission_complete":"failed");
    expect((await coordinator.action(ordinary.actionId))?.sendAccepted).toBe(verified);
    expect(events.some(event=>event.type==="connector_verified")).toBeFalse();
    await coordinator.close();
  }
});

test("omitting an unavailable connector renews the next submission phase without claiming verification",async()=>{
  class OptionalConnectorDriver extends DeferredDriver {
    override async execute(execution:DriverExecution):Promise<void>{
      execution.emit({type:"model_verified",model:execution.request.model!,effort:execution.request.effort!});
      await Bun.sleep(100);
      execution.signal.throwIfAborted();
      execution.emit({type:"connector_omitted"});
      await Bun.sleep(100);
      execution.signal.throwIfAborted();
      execution.emit({type:"send_accepted"});
      execution.emit({type:"submission_complete",canonicalUrl:execution.request.expectedUrl});
    }
  }
  const coordinator=new SingleWindowCoordinator(new OptionalConnectorDriver(),TEST_BUILD,{operationMs:150,cleanupMs:40,hardStopMs:30});
  const ordinary=request('omitted-connector-progress');
  await coordinator.execute(ordinary);const events=await collect(coordinator,ordinary.actionId);
  expect(events.at(-1)?.type).toBe('submission_complete');
  expect(events.some(event=>event.type==='connector_verified')).toBeFalse();
  await coordinator.close();
});

test("ordinary sends still require model verification before connector verification",async()=>{
  class UnverifiedDriver extends DeferredDriver {
    override async execute(execution:DriverExecution):Promise<void>{execution.emit({type:"connector_verified"});execution.emit({type:"send_accepted"});}
  }
  const coordinator=new SingleWindowCoordinator(new UnverifiedDriver(),TEST_BUILD),ordinary=request("unverified-ordinary");
  await coordinator.execute(ordinary);const events=[];for await(const event of coordinator.events(ordinary.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe("failed");expect((await coordinator.action(ordinary.actionId))?.sendAccepted).toBeFalse();
});


test("a stopped browser is not reported as a ChatGPT authentication failure",async()=>{
  const driver=new DeferredDriver();driver.authenticated=false;driver.status="unavailable";
  const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD);
  await coordinator.execute(request("browser-closed"));
  const failure=(await collect(coordinator,"browser-closed")).at(-1);
  expect(failure).toMatchObject({type:"failed"});
  expect(failure?.type==="failed" && failure.message.includes("authentication")).toBeFalse();
  expect(driver.runs).toEqual([]);
});

test("passive approval checks yield to active sends and never start another action",async()=>{
  class ApprovalDriver extends DeferredDriver {
    checks=0;
    async approvePendingConversation(){this.checks++;return true;}
  }
  const driver=new ApprovalDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD);
  const action=request("approval-yields");
  await coordinator.execute(action);
  while(!driver.releases.has(action.actionId))await Bun.sleep(1);
  const approval={url:action.url,expectedAccountFingerprint:"account-1",connectorName:"Codexify"};
  expect(await coordinator.approvePendingConversation(approval)).toMatchObject({approved:false});
  expect(driver.checks).toBe(0);expect((await coordinator.health()).activeActionId).toBe(action.actionId);
  driver.releases.get(action.actionId)!();await collect(coordinator,action.actionId);
  expect(await coordinator.approvePendingConversation(approval)).toMatchObject({approved:true});
  expect(driver.checks).toBe(1);expect(driver.runs).toEqual([action.actionId]);
  await coordinator.close();
});

const shortBudget = {operationMs:45,cleanupMs:40,hardStopMs:30};

test('slow navigation, connector selection and Send acceptance get the 30-second budget',async()=>{
 // Scale the production budget and browser delays together to exercise a
 // 27-second stage without making the test wait in real time.
 const scale=100;
 class SlowSubmissionDriver extends DeferredDriver {
  override async execute(execution:DriverExecution) {
   this.runs.push(execution.request.actionId);
   const slowStage=async()=>{await new Promise(resolve=>setTimeout(resolve,27_000/scale));execution.signal.throwIfAborted();};
   execution.emit({type:'navigating'});await slowStage();
   execution.emit({type:'composer_ready'});
   execution.emit({type:'model_verified',model:execution.request.model!,effort:execution.request.effort!});await slowStage();
   execution.emit({type:'connector_verified'});await slowStage();
   execution.emit({type:'send_accepted'});
   execution.emit({type:'submission_complete',canonicalUrl:execution.request.expectedUrl});
  }
 }
 const driver=new SlowSubmissionDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,{...shortBudget,operationMs:BROWSER_OPERATION_TIMEOUT_MS/scale});
 await coordinator.execute(request('slow-submission'));
 const events=await collect(coordinator,'slow-submission');
 expect(events.at(-1)?.type).toBe('submission_complete');
 expect((await coordinator.action('slow-submission'))?.sendAccepted).toBeTrue();
 expect(driver.runs).toEqual(['slow-submission']);
 await coordinator.close();
});

test('verified submission phases each receive the operation timeout budget',async()=>{
 class ProgressDriver extends DeferredDriver {
  override async execute(execution:DriverExecution) {
   execution.emit({type:'navigating'});
   for(const event of [
    {type:'composer_ready'},
    {type:'model_verified',model:execution.request.model!,effort:execution.request.effort!},
    {type:'connector_verified'},
   ] as const) {
    await new Promise(resolve=>setTimeout(resolve,70));
    execution.signal.throwIfAborted();
    execution.emit(event);
   }
   await new Promise(resolve=>setTimeout(resolve,70));
   execution.signal.throwIfAborted();
   execution.emit({type:'send_accepted'});
   execution.emit({type:'submission_complete',canonicalUrl:execution.request.expectedUrl});
  }
 }
 const coordinator=new SingleWindowCoordinator(new ProgressDriver(),TEST_BUILD,{...shortBudget,operationMs:120});
 await coordinator.execute(request('progressing-submission'));
 const events=await collect(coordinator,'progressing-submission');
 expect(events.at(-1)?.type).toBe('submission_complete');
 expect((await coordinator.action('progressing-submission'))?.sendAccepted).toBeTrue();
 await coordinator.close();
});

test('bounded Stop milestones renew the deadline without response output or Send acceptance',async()=>{
 class StopProgressDriver extends DeferredDriver {
  override async execute(execution:DriverExecution){
   execution.emit({type:'navigating'});
   for(const step of ['initial','retry','recovery','complete'] as const){
    await new Promise(resolve=>setTimeout(resolve,80));
    execution.signal.throwIfAborted();
    execution.emit({type:'stop_progress',step});
    execution.emit({type:'composer_ready'});
   }
   execution.emit({type:'model_verified',model:execution.request.model!,effort:execution.request.effort!});
   execution.emit({type:'connector_verified'});
   execution.emit({type:'send_accepted'});
   execution.emit({type:'submission_complete',canonicalUrl:execution.request.expectedUrl});
  }
 }
 const coordinator=new SingleWindowCoordinator(new StopProgressDriver(),TEST_BUILD,{...shortBudget,operationMs:200});
 await coordinator.execute(request('stop-progress'));
 const events=await collect(coordinator,'stop-progress');
 expect(events.at(-1)?.type).toBe('submission_complete');
 expect(events.some(event=>(event.type as string)==='stop_progress')).toBeFalse();
 expect(events.filter(event=>event.type==='send_accepted')).toHaveLength(1);
 await coordinator.close();
});

for(const phase of ['composer_ready','stop_progress'] as const)test(`repeating ${phase} cannot keep a stalled action alive`,async()=>{
 class RepeatingDriver extends DeferredDriver {
  override async execute(execution:DriverExecution) {
   execution.emit({type:'navigating'});
   for(let attempt=0;attempt<30;attempt++) {
    execution.signal.throwIfAborted();
    execution.emit(phase==='stop_progress'?{type:'stop_progress',step:'initial'}:{type:'composer_ready'});
    await new Promise(resolve=>setTimeout(resolve,10));
   }
  }
 }
 const coordinator=new SingleWindowCoordinator(new RepeatingDriver(),TEST_BUILD,shortBudget);
 await coordinator.execute(request('repeated-phase'));
 const events=await collect(coordinator,'repeated-phase');
 expect(events.at(-1)?.type).toBe('failed');
 expect((events.at(-1) as any).message).toContain('timed out');
 await coordinator.close();
});

class TimedSubmissionDriver extends DeferredDriver {
  attempted = false;
  constructor(private readonly phase:'before-send'|'attempted'|'accepted') { super(); }
  submissionMayHaveBeenAccepted(id:string) { return this.attempted && id==='timed-submission'; }
  override async execute(execution:DriverExecution) {
    this.runs.push(execution.request.actionId);
    execution.emit({type:'navigating'});
    if(execution.request.model!==null&&execution.request.effort!==null)execution.emit({type:'model_verified',model:execution.request.model,effort:execution.request.effort});
    execution.emit({type:'connector_verified'});
    if(execution.request.actionId!=='timed-submission') {execution.emit({type:'send_accepted'});execution.emit({type:'submission_complete',canonicalUrl:execution.request.expectedUrl});return;}
    this.attempted=this.phase!=='before-send';
    if(this.phase==='accepted')execution.emit({type:'send_accepted'});
    await new Promise<void>((resolve,reject)=>{
      this.releases.set(execution.request.actionId,resolve);
      execution.signal.addEventListener('abort',()=>reject(new DOMException('aborted','AbortError')),{once:true});
    });
    execution.emit({type:'submission_complete',canonicalUrl:execution.request.expectedUrl});
  }
}
for(const phase of ['before-send','attempted','accepted'] as const)test(`action deadline releases ownership and preserves ${phase} Send uncertainty`,async()=>{
 const driver=new TimedSubmissionDriver(phase),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,shortBudget);
 await coordinator.execute(request('timed-submission'));
 const events=await collect(coordinator,'timed-submission');
 expect(events.at(-1)?.type).toBe(phase==='before-send'?'failed':'ambiguous_send');
 expect((events.at(-1) as any).message).toContain('timed out');
 expect((await coordinator.health()).activeActionId).toBeNull();
 const snapshot=await coordinator.action('timed-submission');
 expect((await coordinator.execute(request('timed-submission'))).state).toBe(snapshot!.state);
 await coordinator.execute(request('next-submission'));await collect(coordinator,'next-submission');
 expect(driver.runs).toEqual(['timed-submission','next-submission']);
 expect((await coordinator.action('timed-submission'))?.state).toBe(snapshot!.state);
 await coordinator.close();
});

test('maintenance deadlines abort their work and release the window operation queue',async()=>{
 const driver=new DeferredDriver();let signalSeen:AbortSignal|undefined;
 (driver as BrowserDriver).refreshConnector=async(_name,signal)=>{
  signalSeen=signal;
  await new Promise<void>((_resolve,reject)=>signal!.addEventListener('abort',()=>reject(new DOMException('aborted','AbortError')),{once:true}));
  throw Error('unreachable');
 };
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,shortBudget);
 const stalled=coordinator.refreshConnector('Fixture connector');
 const next=coordinator.capabilities();
 await expect(stalled).rejects.toThrow('timed out');
 expect(signalSeen!.aborted).toBeTrue();
 expect((await next).accountFingerprint).toBe('account-1');
 expect(driver.capabilityCalls).toBe(1);
 await coordinator.close();
});

test('a driver that ignores cancellation is retired before its tab can be reused',async()=>{
 const driver=new DeferredDriver();let closed=0;
 (driver as BrowserDriver).refreshConnector=async()=>new Promise(()=>{});
 (driver as BrowserDriver).close=async()=>{closed++;};
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,shortBudget);
 await expect(coordinator.refreshConnector('Fixture connector')).rejects.toThrow('timed out');
 expect(closed).toBe(1);
 expect((await coordinator.health()).status).toBe('unavailable');
 await expect(coordinator.capabilities()).rejects.toThrow('recovery is required');
 expect(driver.capabilityCalls).toBe(0);
 await coordinator.close();expect(closed).toBe(1);
});

test('a stalled admitted preflight records a definite unsent failure and retires its driver',async()=>{
 const driver=new DeferredDriver();let closed=0;
 driver.health=async()=>new Promise(()=>{});
 (driver as BrowserDriver).close=async()=>{closed++;};
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,shortBudget);
 await coordinator.execute(request('stalled-preflight'));
 const events=await collect(coordinator,'stalled-preflight');
 expect(events.at(-1)).toMatchObject({type:'failed',message:expect.stringContaining('timed out')});
 expect(await coordinator.action('stalled-preflight')).toMatchObject({state:'failed',sendAccepted:false});
 expect(driver.runs).toEqual([]);expect(closed).toBe(1);
 expect((await coordinator.health()).status).toBe('unavailable');
 await coordinator.close();
});

test('connector maintenance retains the UI lease across steps and restoration',async()=>{
 const driver=new DeferredDriver(),order:string[]=[];let entered!:()=>void,release!:()=>void;
 const started=new Promise<void>(resolve=>{entered=resolve;}),gate=new Promise<void>(resolve=>{release=resolve;});
 const original=driver.capabilities.bind(driver);
 driver.capabilities=async()=>{order.push('other-operation');return original();};
 (driver as BrowserDriver).refreshConnectorSteps=async(_name,run,signal)=>{
  await run(async()=>{order.push('first-step');entered();await gate;},signal);
  await run(async()=>{order.push('second-step');},signal);
  return {appId:'asdk_app_fixture',refreshObserved:true,permissionChanged:false,schemaVerified:false as const};
 };
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,shortBudget);
 const refresh=coordinator.refreshConnector('Fixture connector');await started;
 expect((await coordinator.health()).status).toBe('busy');
 const other=coordinator.capabilities();release();
 await refresh;await other;
 expect(order).toEqual(['first-step','second-step','other-operation']);
 expect((await coordinator.health()).status).toBe('idle');await coordinator.close();
});

test('refresh observation and restoration exclude plan checks, navigation and submissions',async()=>{
 const driver=new DeferredDriver(),order:string[]=[];
 let observing!:()=>void,confirm!:()=>void,restoring!:()=>void,restored!:()=>void;
 const observationStarted=new Promise<void>(resolve=>{observing=resolve;});
 const confirmation=new Promise<void>(resolve=>{confirm=resolve;});
 const restorationStarted=new Promise<void>(resolve=>{restoring=resolve;});
 const restoration=new Promise<void>(resolve=>{restored=resolve;});
 (driver as BrowserDriver).refreshConnectorSteps=async(_name,run,signal)=>{
  await run(async()=>{order.push('prepare-refresh');},signal);
  observing();await confirmation;order.push('confirmed');
  await run(async()=>{restoring();await restoration;order.push('restored');});
  return {appId:'asdk_app_fixture',refreshObserved:true,permissionChanged:false,schemaVerified:false as const};
 };
 (driver as BrowserDriver).accountPlan=async()=>{order.push('plan');return {accountFingerprint:'account-1',kind:'pro',label:'Pro 20x',sourceLabel:'ChatGPT Pro 200'};};
 const capabilities=driver.capabilities.bind(driver);
 driver.capabilities=async()=>{order.push('capabilities');return capabilities();};
 driver.reveal=async()=>{order.push('navigate');};
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,{operationMs:500,cleanupMs:100,hardStopMs:100});
 const refresh=coordinator.refreshConnector('Fixture connector');await observationStarted;
 const plan=coordinator.accountPlan('startup'),catalog=coordinator.capabilities(),navigation=coordinator.reveal('https://chatgpt.com/c/next'),submission=coordinator.execute(request('after-refresh'));
 await Bun.sleep(600); // Longer than a UI phase's watchdog; confirmation has its own deadline.
 expect(order).toEqual(['prepare-refresh']);expect(driver.runs).toEqual([]);
 expect((await coordinator.health()).status).toBe('busy');
 confirm();await restorationStarted;
 expect(order).toEqual(['prepare-refresh','confirmed']);expect(driver.runs).toEqual([]);
 restored();await refresh;await Promise.all([plan,catalog,navigation,submission]);
 expect(order).toEqual(['prepare-refresh','confirmed','restored','plan','capabilities','navigate']);
 while(!driver.releases.has('after-refresh'))await Bun.sleep(1);
 driver.releases.get('after-refresh')!();await collect(coordinator,'after-refresh');
 await coordinator.close();
},3000);

test('cancelled refresh restores its page before queued account work can run',async()=>{
 const driver=new DeferredDriver(),order:string[]=[];let observing!:()=>void,restoring!:()=>void,restored!:()=>void;
 const observationStarted=new Promise<void>(resolve=>{observing=resolve;});
 const restorationStarted=new Promise<void>(resolve=>{restoring=resolve;});
 const restoration=new Promise<void>(resolve=>{restored=resolve;});
 (driver as BrowserDriver).refreshConnectorSteps=async(_name,run,signal)=>{
  await run(async()=>{},signal);observing();
  try {await new Promise<void>((_resolve,reject)=>signal!.addEventListener('abort',()=>reject(signal!.reason),{once:true}));}
  finally {await run(async cleanupSignal=>{expect(cleanupSignal.aborted).toBeFalse();restoring();await restoration;order.push('restored');});}
  throw Error('unreachable');
 };
 (driver as BrowserDriver).accountPlan=async()=>{order.push('plan');return {accountFingerprint:'account-1',kind:'pro',label:'Pro 20x',sourceLabel:'ChatGPT Pro 200'};};
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,{operationMs:500,cleanupMs:100,hardStopMs:100});
 const cancellation=new AbortController(),refresh=coordinator.refreshConnector('Fixture connector',cancellation.signal);
 const rejected=refresh.then(()=>{throw Error('Expected refresh cancellation');},error=>{expect(error.message).toContain('cancelled');});await observationStarted;
 const plan=coordinator.accountPlan('queued');cancellation.abort();await restorationStarted;
 expect(order).toEqual([]);expect((await coordinator.health()).status).toBe('busy');
 restored();await rejected;await plan;expect(order).toEqual(['restored','plan']);await coordinator.close();
},2000);

test("liveness checks browser responsiveness without inspecting account sessions", async () => {
  class LivenessDriver extends DeferredDriver {
    available = true;
    healthCalls = 0;
    async liveness() { return this.available; }
    async health(): ReturnType<DeferredDriver["health"]> { this.healthCalls++; throw new Error("Session API is unavailable"); }
  }
  const driver = new LivenessDriver(), coordinator = new SingleWindowCoordinator(driver, TEST_BUILD);
  expect((await coordinator.liveness()).available).toBe(true);
  driver.available = false;
  expect((await coordinator.liveness()).available).toBe(false);
  expect(driver.healthCalls).toBe(0);
});

test("liveness leaves a bounded active operation to its normal deadline", async () => {
  class OperationDriver extends DeferredDriver {
    probes = 0;
    async liveness() { this.probes++; return false; }
  }
  const driver=new OperationDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD);
  await coordinator.execute(request("liveness-active"));
  await new Promise(resolve=>setTimeout(resolve,0));
  expect((await coordinator.liveness()).available).toBe(true);
  expect(driver.probes).toBe(0);
  driver.releases.get("liveness-active")?.();
  await new Promise(resolve=>setTimeout(resolve,0));
  expect((await coordinator.liveness()).available).toBe(false);
  expect(driver.probes).toBe(1);
});

for(const sent of [false,true])test(`cooldown deferral preserves ${sent?'accepted Send uncertainty':'the pending unsent action'}`,async()=>{
 class WaitingDriver extends DeferredDriver {
  override async execute(execution:DriverExecution){
   execution.emit({type:'navigating'});
   if(sent){
    execution.emit({type:'model_verified',model:'gpt-6-sol',effort:'high'});
    execution.emit({type:'connector_verified'});execution.emit({type:'send_accepted'});
   }
   throw new ConversationCooldownError(Date.now()+180000);
  }
 }
 const coordinator=new SingleWindowCoordinator(new WaitingDriver(),TEST_BUILD);
 await coordinator.execute(request('waiting-send'));
 expect((await collect(coordinator,'waiting-send')).at(-1)?.type).toBe(sent?'ambiguous_send':'waiting');
 const snapshot=await coordinator.action('waiting-send');
 expect(snapshot?.sendAccepted).toBe(sent);
 expect(snapshot?.cooldownUntilMs!==undefined).toBe(!sent);
 expect((await coordinator.health()).activeActionId).toBeNull();
 await coordinator.close();
});

for(const accepted of [false,true])test(`conversation length limit retains its terminal cause after accepted=${accepted}`,async()=>{
 const driver=new DeferredDriver();driver.execute=async execution=>{
  driver.runs.push(execution.request.actionId);
  if(accepted){execution.emit({type:'model_verified',model:'gpt-6-sol',effort:'high'});execution.emit({type:'send_accepted'});}
  throw new BrowserCompanionError('Conversation length limit','conversation_limit',{sendAccepted:accepted});
 };
 const coordinator=new SingleWindowCoordinator(driver,TEST_BUILD),id='full-'+accepted;
 await coordinator.execute(request(id));const events=await collect(coordinator,id),snapshot=await coordinator.action(id);
 expect(events.at(-1)).toMatchObject({type:'failed',code:'conversation_limit'});
 expect(snapshot).toMatchObject({state:'failed',sendAccepted:accepted,attention:{code:'conversation_limit'}});
 expect(driver.runs).toEqual([id]);await coordinator.close();
});

class PairingWindowDriver extends DeferredDriver {
  maintenanceCalls = 0;
  async accountPlan() { this.maintenanceCalls++; return {label:"Pro",sourceLabel:"Pro",kind:"pro" as const,accountFingerprint:"account-1"}; }
  async prepareNewChat() { this.maintenanceCalls++; return this.capabilities(); }
  async refreshConnector() { this.maintenanceCalls++; return {appId:"app",refreshObserved:true,permissionChanged:false,schemaVerified:false as const}; }
  override async execute(execution:DriverExecution) {
    this.runs.push(execution.request.actionId);
    if (execution.request.model && execution.request.effort) execution.emit({type:"model_verified",model:execution.request.model,effort:execution.request.effort});
    execution.emit({type:"send_accepted"});
    const canonicalUrl = execution.request.prompt.kind === "create_chat" ? "https://chatgpt.com/c/paired-window" : execution.request.expectedUrl;
    if (execution.request.prompt.kind === "create_chat") execution.emit({type:"conversation_created",canonicalUrl});
    execution.emit({type:"submission_complete",canonicalUrl});
  }
}

const retainedPairing = ():ExecuteActionRequest=>({...request("pair-window"),url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",
  model:null,effort:null,retainPairingWindow:true,prompt:{kind:"create_chat",continuationToken:"A".repeat(43),reference:Buffer.alloc(32,1).toString('base64url'),setupReference:null}});

test("re-pairing retains the browser through verification and admits only its matching continuation",async()=>{
  const driver=new PairingWindowDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD);
  await coordinator.execute(retainedPairing());
  for await(const _ of coordinator.events("pair-window",0)){}
  expect((await coordinator.health()).activeActionId).toBe("pair-window");
  await expect(coordinator.execute(request("other"))).rejects.toThrow("busy");
  await expect(coordinator.accountPlan("epoch")).rejects.toThrow("busy");
  await expect(coordinator.newChatCapabilities(null)).rejects.toThrow("busy");
  await expect(coordinator.refreshConnector("Codexify")).rejects.toThrow("busy");
  expect(driver.maintenanceCalls).toBe(0);
  const followUp:ExecuteActionRequest={...request("normal-prompt"),url:"https://chatgpt.com/c/paired-window",expectedUrl:"https://chatgpt.com/c/paired-window",pairingContinuationOf:"pair-window"};
  for(const change of [{url:"https://chatgpt.com/c/other"},{pairingGeneration:2},{expectedAccountFingerprint:"other-account"},{connectorName:"Other connector"},{pairingContinuationOf:"other-pair"}]) {
    await expect(coordinator.execute({...followUp,...change})).rejects.toThrow("busy");
  }
  // A still-generating initialization response belongs to this reservation.
  driver.status="busy";
  await coordinator.execute(followUp);
  for await(const _ of coordinator.events(followUp.actionId,0)){}
  expect(driver.runs).toEqual(["pair-window","normal-prompt"]);
  expect((await coordinator.health()).activeActionId).toBeNull();
  await coordinator.close();
});

test("re-pairing reservation expires or releases without replaying Send",async()=>{
  for(const mode of ["timeout","release","shutdown"] as const) {
    const driver=new PairingWindowDriver(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,{operationMs:1000,cleanupMs:100,hardStopMs:100,pairingRetentionMs:30});
    await coordinator.execute(retainedPairing());
    for await(const _ of coordinator.events("pair-window",0)){}
    expect(coordinator.releasePairingWindow("wrong-action")).toBeFalse();
    if(mode==="timeout") await Bun.sleep(60);
    else if(mode==="release") expect(coordinator.releasePairingWindow("pair-window")).toBeTrue();
    else await coordinator.close();
    expect((await coordinator.health()).activeActionId).toBeNull();
    expect(driver.runs).toEqual(["pair-window"]);
    if(mode!=="shutdown") await coordinator.close();
  }
});

test("a first-work failure before Send keeps the reserved page and a renewed lease",async()=>{
  class FirstWorkFailure extends PairingWindowDriver {
    fail=true;
    override async execute(execution:DriverExecution){
      if(execution.request.prompt.kind!=="create_chat"&&this.fail){
        this.fail=false;this.runs.push(execution.request.actionId);
        throw new BrowserCompanionError("Model could not be selected","model_unavailable");
      }
      await super.execute(execution);
    }
  }
  const driver=new FirstWorkFailure(),coordinator=new SingleWindowCoordinator(driver,TEST_BUILD,{operationMs:1000,cleanupMs:100,hardStopMs:100,pairingRetentionMs:200});
  await coordinator.execute(retainedPairing());for await(const _ of coordinator.events("pair-window",0)){}
  const first={...request("first-work-failure"),url:"https://chatgpt.com/c/paired-window",expectedUrl:"https://chatgpt.com/c/paired-window",pairingContinuationOf:"pair-window"};
  await coordinator.execute(first);for await(const _ of coordinator.events(first.actionId,0)){}
  expect((await coordinator.health()).activeActionId).toBe("pair-window");
  await expect(coordinator.execute(request("other"))).rejects.toThrow("busy");
  await expect(coordinator.accountPlan("epoch")).rejects.toThrow("busy");
  await Bun.sleep(120);expect(coordinator.refreshPairingWindow("pair-window")).toBeTrue();
  expect(coordinator.refreshPairingWindow("wrong-action")).toBeFalse();
  await Bun.sleep(120);expect((await coordinator.health()).activeActionId).toBe("pair-window");
  const retry={...first,actionId:"first-work-retry",idempotencyKey:"first-work-retry"};
  await coordinator.execute(retry);for await(const _ of coordinator.events(retry.actionId,0)){}
  expect((await coordinator.health()).activeActionId).toBeNull();
  expect(driver.runs).toEqual(["pair-window","first-work-failure","first-work-retry"]);
  await coordinator.close();
});
