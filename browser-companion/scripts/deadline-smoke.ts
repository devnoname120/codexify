import assert from "node:assert/strict";
import {SingleWindowCoordinator, type BrowserDriver} from "../src/browser-window";
import {protocolBuild, type ExecuteActionRequest} from "../src/protocol";

const build = protocolBuild("deadline-smoke");
const watchdog = setTimeout(() => { console.error("Browser deadline did not release ownership"); process.exit(1); }, 5_000);
try {
  for (const phase of ["before-send", "attempted", "accepted"] as const) {
    let attempted = false;
    const driver: BrowserDriver = {
      async health() { return {authenticated:true, accountFingerprint:"account-1", status:"idle", detail:null}; },
      async login() { throw new Error("Unexpected login"); },
      async capabilities() { throw new Error("Unexpected capabilities"); },
      async reveal() {},
      submissionMayHaveBeenAccepted() { return attempted; },
      async execute({request, signal, emit}) {
        emit({type:"model_verified", model:"gpt-6-sol", effort:"high"});
        emit({type:"connector_verified"});
        if (request.actionId === "next") {
          emit({type:"send_accepted"});
          emit({type:"submission_complete", canonicalUrl:request.expectedUrl});
          return;
        }
        attempted = phase !== "before-send";
        if (phase === "accepted") emit({type:"send_accepted"});
        await new Promise<void>((_resolve, reject) => {
          signal.addEventListener("abort", () => reject(signal.reason), {once:true});
        });
      },
    };
    const coordinator = new SingleWindowCoordinator(driver, build, {operationMs:60, cleanupMs:40, hardStopMs:40});
    const request: ExecuteActionRequest = {
      actionId:"stalled", idempotencyKey:"stalled-key", pairingGeneration:1,
      url:"https://chatgpt.com/c/fixture", expectedUrl:"https://chatgpt.com/c/fixture",
      expectedAccountFingerprint:"account-1", connectorName:"Codexify",
      model:"gpt-6-sol", effort:"high", autoApproveToolCalls:false,
      prompt:{kind:"offline_goal", goalRevision:1},
    };
    await coordinator.execute(request);
    const events = [];
    for await (const event of coordinator.events(request.actionId, 0)) events.push(event);
    assert.equal(events.at(-1)?.type, phase === "before-send" ? "failed" : "ambiguous_send");
    assert.equal((await coordinator.health()).activeActionId, null);
    await coordinator.execute({...request, actionId:"next", idempotencyKey:"next-key"});
    const next = [];
    for await (const event of coordinator.events("next", 0)) next.push(event);
    assert.equal(next.at(-1)?.type, "submission_complete");
    await coordinator.close();
  }
  console.log(JSON.stringify({ok:true, phases:3}));
} finally { clearTimeout(watchdog); }
