export type BrowserFailureCode =
  | "authentication_required"
  | "approval_required"
  | "rate_limited"
  | "conversation_cooldown"
  | "external_interference"
  | "ambiguous_send"
  | "selector_drift"
  | "conversation_unavailable"
  | "editor_unavailable"
  | "model_unavailable"
  | "connector_unavailable"
  | "cancelled"
  | "failed";

// Native recovery counts distinct confirmed-unsent loads with this diagnosis.
export const CONVERSATION_UNAVAILABLE_MESSAGE = "ChatGPT conversation temporarily not accessible";

export class BrowserCompanionError extends Error {
  readonly code: BrowserFailureCode;
  readonly sendAccepted: boolean;
  readonly cause?: unknown;

  constructor(message: string, code: BrowserFailureCode = "failed", options: { sendAccepted?: boolean; cause?: unknown } = {}) {
    super(message);
    this.name = "BrowserCompanionError";
    this.code = code;
    this.sendAccepted = options.sendAccepted ?? false;
    this.cause = options.cause;
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
