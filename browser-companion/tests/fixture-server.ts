import type { CapabilityCatalog } from "../src/protocol";
import { SingleWindowCoordinator, type BrowserDriver, type DriverExecution } from "../src/browser-window";
import { startControlServer } from "../src/control-server";
import { protocolBuild, validateProtocolBuild } from "../src/protocol";
import { ConversationCooldownError } from "../src/errors";
import {BrowserCompanionError} from "../src/errors";

const descriptorPath = process.argv[2];
const build = process.argv[3]
  ? validateProtocolBuild(process.argv[3])
  : protocolBuild("contract-build");
const mode = process.argv[4] ?? "immediate";
if (!descriptorPath) throw new Error("descriptor path is required");

class ContractDriver implements BrowserDriver {
  async health() {
    if (mode === "slow-preflight") await Bun.sleep(1_500);
    return {
      authenticated:true,
      accountFingerprint:"contract-account",
      status:"idle" as const,
      detail:null,
    };
  }
  async login() { return this.health(); }
  async navigationAvailability() {return 180_000;}
  async capabilities(): Promise<CapabilityCatalog> {
    if(mode==="picker-error")throw new BrowserCompanionError("ChatGPT did not expose one owned model/effort control","selector_drift",{phase:"effort_control_lookup"});
    return {
      protocolVersion:1,
      build,
      accountFingerprint:"contract-account",
      catalogRevision:"contract-catalog",
      models:[{
        id:"gpt-6-sol",
        label:"GPT-6 Sol",
        efforts:[{ id:"high", label:"High", available:true, reason:null }],
      }],
    };
  }
  async reveal() {}
  async execute(execution: DriverExecution) {
    execution.emit({ type:"navigating" });
    if(mode === "waiting")throw new ConversationCooldownError(Date.now()+180000);
    execution.emit({ type:"composer_ready" });
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    execution.emit({ type:"status_delta", text:"Working", continuation:false });
    execution.emit({ type:"commentary_delta", markdown:"Visible commentary", continuation:false });
    execution.emit({ type:"answer_delta", markdown:"Contract answer", continuation:false });
    execution.emit({
      type:"generation_complete",
      finalMarkdown:"Contract answer",
      canonicalUrl:execution.request.expectedUrl,
    });
  }
}

class BlockingContractDriver extends ContractDriver {
  override async execute(execution: DriverExecution) {
    execution.emit({ type:"navigating" });
    if (execution.request.model !== null && execution.request.effort !== null) execution.emit({ type:"model_verified", model:execution.request.model, effort:execution.request.effort });
    execution.emit({ type:"connector_verified" });
    execution.emit({ type:"send_accepted" });
    await new Promise<void>((_resolve, reject) => {
      execution.signal.addEventListener(
        "abort",
        () => reject(new DOMException("aborted", "AbortError")),
        { once:true },
      );
    });
  }
}

const driver = mode === "blocking" ? new BlockingContractDriver() : new ContractDriver();
const coordinator = new SingleWindowCoordinator(driver, build);
const server = await startControlServer(coordinator, descriptorPath);
console.log(JSON.stringify({ ready:true, endpoint:server.endpoint }));

let stopping = false;
async function stop() {
  if (stopping) return;
  stopping = true;
  await server.close();
  process.exit(0);
}
process.once("SIGINT", () => { void stop(); });
process.once("SIGTERM", () => { void stop(); });
