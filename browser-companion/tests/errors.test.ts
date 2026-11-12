import { expect, test } from "bun:test";
import { asCompanionError, BrowserCompanionError,ModelOptionUnavailableError,browserStage } from "../src/errors";

test("unknown browser failures never expose raw page or request data", () => {
  const error = asCompanionError(new Error(
    "navigation failed at https://chatgpt.com/c/private-conversation with private browser prompt",
  ));

  expect(error.code).toBe("failed");
  expect(error.message).toBe("Browser companion operation failed");
  expect(error.message).not.toContain("private-conversation");
  expect(error.message).not.toContain("private browser prompt");
});

test("controlled companion failures preserve their bounded operational message", () => {
  const original = new BrowserCompanionError(
    "ChatGPT requires one-time approval in the shared browser window",
    "approval_required",
    { sendAccepted:true },
  );

  expect(asCompanionError(original)).toBe(original);
});

test("picker stages retain a safe diagnosis without exposing a raw browser failure",async()=>{
  const original=new Error("https://chatgpt.com/c/private-conversation private browser prompt bearer-secret");
  const result=await browserStage("effort_control_lookup","ChatGPT did not expose its effort control",async()=>{throw original;}).catch(error=>error);
  expect(result).toMatchObject({code:"selector_drift",phase:"effort_control_lookup",sendAccepted:false});
  expect(result.cause).toBe(original);
  expect(result.message).toBe("ChatGPT did not expose its effort control (step: effort_control_lookup)");
  expect(result.message).not.toContain("private");expect(result.message).not.toContain("bearer-secret");
});

test("known picker causes remain actionable in stage diagnostics",async()=>{
  const message="ChatGPT did not announce the selected effort's label and position";
  const error=await browserStage("effort_selection","Effort discovery failed",async()=>{throw new Error(message);}).catch(error=>error);
  expect(error.message).toContain(message);expect(error.phase).toBe("effort_selection");
});

test("adding a stage preserves verified model unavailability and its fallback family",async()=>{
  const original=new ModelOptionUnavailableError("ChatGPT model 6 is unavailable","6");
  const failure=await browserStage("model_options","Model inspection failed",async()=>{throw original;}).catch(error=>error);
  expect(failure).toBe(original);expect(failure).toBeInstanceOf(ModelOptionUnavailableError);
  expect(failure.unavailableFamily).toBe("6");expect(failure.phase).toBe("model_options");
});
