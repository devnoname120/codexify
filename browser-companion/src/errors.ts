export type BrowserFailureCode =
  | "authentication_required"
  | "approval_required"
  | "rate_limited"
  | "conversation_load_rate_limited"
  | "conversation_cooldown"
  | "external_interference"
  | "ambiguous_send"
  | "selector_drift"
  | "conversation_unavailable"
  | "conversation_limit"
  | "editor_unavailable"
  | "model_unavailable"
  | "connector_unavailable"
  | "cancelled"
  | "failed";

export const BROWSER_STAGES = ["composer_lookup", "effort_control_lookup", "picker_open", "model_options", "model_identity", "effort_selection", "effort_restore", "capability_discovery"] as const;
export type BrowserStage = typeof BROWSER_STAGES[number];
const pickerFailureReasons=new Set([
  "ChatGPT effort control did not expose its owned menu or structural slider after click and primary pointerdown",
  "ChatGPT effort slider exposed an invalid ARIA range",
  "ChatGPT effort availability could not be verified from its slider ticks",
  "ChatGPT did not announce the selected effort's label and position",
  "ChatGPT changed its effort range or availability",
  "ChatGPT locks the requested effort",
  "ChatGPT did not expose any discoverable effort labels",
  "ChatGPT exposed duplicate effort labels",
  "ChatGPT capability discovery requires an empty composer",
]);

export class BrowserCompanionError extends Error {
  readonly code: BrowserFailureCode;
  readonly sendAccepted: boolean;
  readonly cause?: unknown;
  readonly phase?: BrowserStage;

  constructor(message: string, code: BrowserFailureCode = "failed", options: { sendAccepted?: boolean; cause?: unknown; phase?:BrowserStage } = {}) {
    super(message);
    this.name = "BrowserCompanionError";
    this.code = code;
    this.sendAccepted = options.sendAccepted ?? false;
    this.cause = options.cause;
    this.phase = options.phase;
  }
}

/** Public stage messages are supplied by code, never by a raw page/request error. */
export async function browserStage<T>(phase:BrowserStage,message:string,operation:()=>Promise<T>):Promise<T> {
  try { return await operation(); }
  catch (cause) {
    const failure=asCompanionError(cause);
    if(failure.phase || failure.code==="cancelled")throw failure;
    const reason=cause instanceof Error&&pickerFailureReasons.has(cause.message)?`: ${cause.message}`:"";
    // Preserve typed model unavailability: fallback is permitted only for that
    // subclass, never for an unknown picker or transport failure.
    const error=failure.code==="failed"
      ?new BrowserCompanionError(`${message+reason} (step: ${phase})`,"selector_drift",{phase,sendAccepted:failure.sendAccepted,cause})
      :Object.assign(failure,{phase,message:`${failure.message} (step: ${phase})`});
    console.error(`[browser-picker] ${JSON.stringify({phase,code:error.code,message:error.message,
      causeType:cause instanceof Error&&["TimeoutError","AbortError"].includes(cause.name)?cause.name:"Error"})}`);
    throw error;
  }
}

export class ConversationCooldownError extends BrowserCompanionError {
  constructor(readonly availableAtMs:number) {
    super("Waiting for the shared browser’s conversation-loading cooldown", "conversation_cooldown");
  }
}

export function selectorDrift(message: string, cause?: unknown): BrowserCompanionError {
  return new BrowserCompanionError(message, "selector_drift", { cause });
}

/** Only a picker-confirmed missing/locked option permits an automatic fallback. */
export class ModelOptionUnavailableError extends BrowserCompanionError {
  constructor(message:string,readonly unavailableFamily?:string) {
    super(message,"model_unavailable");
  }
}

export function asCompanionError(error: unknown): BrowserCompanionError {
  if (error instanceof BrowserCompanionError) return error;
  if (error instanceof DOMException && error.name === "AbortError") {
    return new BrowserCompanionError("Browser action was cancelled", "cancelled", { cause:error });
  }
  return new BrowserCompanionError("Browser companion operation failed", "failed", { cause:error });
}
