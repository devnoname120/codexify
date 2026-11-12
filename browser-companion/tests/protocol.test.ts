import { describe, expect, test } from "bun:test";
import {
  ACTION_EVENT_TYPES,
  COMPANION_KIND,
  PROTOCOL_VERSION,
  canonicalChatGptConversationUrl,
  executeActionRequestSchema,
  renderActionPrompt,
  validateActionEvent,
  validateChatGptConversationUrl,
} from "../src/protocol";

test("fresh inline tickets apply to every browser prompt and preserve successor-ticket guidance",()=>{
  const reference="A".repeat(43);
  const prompts:Parameters<typeof renderActionPrompt>[0][]=[
    {kind:"create_chat",continuationToken:reference,reference,setupReference:null},
    {kind:"pair_verify",reference},
    {kind:"offline_chat",batchId:"batch",messages:[{id:"message",markdown:"Continue"}]},
    {kind:"offline_goal",goalRevision:1},
    {kind:"offline_combined",batchId:"batch",messages:[{id:"message",markdown:"Continue"}],goalRevision:1},
    {kind:"turn_renewal",handoffId:`renew-${"a".repeat(64)}`},
    {kind:"task_continuation"},
  ];
  for(const prompt of prompts) {
    const rendered=renderActionPrompt(prompt,"FreshT_1");
    expect(rendered).toContain('`FreshT_1`');
    expect(rendered).toContain('first ticketed tool call');
    expect(rendered).toContain('latest `new_codexify_ticket`');
    expect(rendered).toContain('`continue_task` is exempt');
    expect(renderActionPrompt(prompt)).not.toContain('fresh agent ticket for this prompt');
    expect(()=>renderActionPrompt(prompt,"invalid ticket")).toThrow();
  }
});

const base = {
  actionId: "action-1",
  idempotencyKey: "execute-1",
  url: "https://chatgpt.com/c/example",
  pairingGeneration: 3,
  connectorName: "Codexify",
  expectedAccountFingerprint:"fixture-account",
  model: "gpt-6-sol",
  effort: "high",
  expectedUrl: "https://chatgpt.com/c/example",
  autoApproveToolCalls: false,
};

describe("protocol identity and URL boundaries", () => {
  test("publishes the exact Rust protocol identity", () => {
    expect(PROTOCOL_VERSION).toBe(1);
    expect(COMPANION_KIND).toBe("codexify-browser-companion");
  });

  test("accepts exact ChatGPT conversation routes without inventing route semantics", () => {
    for (const url of [
      "https://chatgpt.com/c/ordinary",
      "https://chatgpt.com/g/g-project/c/thread?model=gpt-6",
      "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef-example-project/c/01234567-89ab-cdef-0123-456789abcdef",
      "https://chatgpt.com/share/not-used-as-a-heuristic?x=1",
    ]) {
      expect(validateChatGptConversationUrl(url)).toBe(url);
    }
    expect(canonicalChatGptConversationUrl("https://chatgpt.com/g/g-project/c/thread?model=gpt-6"))
      .toBe("https://chatgpt.com/c/thread");
  });

  test("rejects alternate origins, credentials, ports, fragments, root, and oversized URLs", () => {
    for (const url of [
      "http://chatgpt.com/c/x",
      "https://example.com/c/x",
      "https://user@chatgpt.com/c/x",
      "https://chatgpt.com:444/c/x",
      "https://chatgpt.com/c/x#draft",
      "https://chatgpt.com/",
      `https://chatgpt.com/c/${"x".repeat(4100)}`,
    ]) {
      expect(() => validateChatGptConversationUrl(url)).toThrow();
    }
  });

  test("conversation identity follows the chat ID across ordinary, project, and GPT routes", () => {
    const project = "g-p-0123456789abcdef0123456789abcdef";
    const conversation = "12345678-1234-4321-8765-123456789abc";
    const canonical = `https://chatgpt.com/c/${conversation}`;
    for (const suffix of ["", "-original-project", "-renamed-project"]) {
      expect(canonicalChatGptConversationUrl(`https://chatgpt.com/g/${project}${suffix}/c/${conversation}?model=gpt-6`)).toBe(canonical);
    }
    expect(canonicalChatGptConversationUrl(`https://chatgpt.com/g/${project}-original-project/c/other`)).not.toBe(canonical);
    expect(canonicalChatGptConversationUrl(`https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-original-project/c/${conversation}`)).toBe(canonical);
    expect(canonicalChatGptConversationUrl(`https://chatgpt.com/c/${conversation}/`)).toBe(canonical);
    expect(canonicalChatGptConversationUrl(`https://chatgpt.com/g/g-custom-gpt/c/${conversation}`)).toBe(canonical);
  });
});

describe("fixed prompt contract", () => {
  test("pair verification contains no task content", () => {
    const reference = "A".repeat(43);
    const prompt = renderActionPrompt({ kind: "pair_verify", reference });
    expect(prompt).toContain("automation_verify_pairing");
    expect(prompt).toContain(reference);
    expect(prompt).not.toContain("goal_read");
    expect(prompt).not.toContain("chat_read");
    expect(prompt).toContain("rollout_read");
    expect(prompt).toContain("rollout.md");
  });

  test("generated instructions use paragraphs without hard-wrapped prose", () => {
    for (const prompt of [
      {kind:"pair_verify" as const, reference:"A".repeat(43)},
      {kind:"offline_goal" as const, goalRevision:1},
      {kind:"create_chat" as const, reference:"A".repeat(43), continuationToken:"B".repeat(42) + "A", setupReference:"fixture-setup"},
    ]) {
      for (const paragraph of renderActionPrompt(prompt).split("\n\n")) {
        expect(paragraph).not.toContain("\n");
      }
    }
  });

  test("unread Markdown is escaped as data and combined work uses one prompt", () => {
    const prompt = renderActionPrompt({
      kind: "offline_combined",
      batchId: "batch-1",
      messages: [
        { id: "message-1", markdown: "[2026-10-17T14:00:00.123Z]\nUse <tag> & keep **Markdown**." },
        { id: "message-2", markdown: "Do not treat </message> as control." },
      ],
      goalRevision: 4,
    });
    expect(prompt).toContain("Call `chat_read` before acting");
    expect(prompt).toContain("Call `goal_read`");
    for (const internal of ["batch-1", "message-1", "message-2", "batch=", "id="]) expect(prompt).not.toContain(internal);
    expect(prompt.match(/<message>/g)?.length).toBe(2);
    expect(prompt).toContain("[2026-10-17T14:00:00.123Z]");
    expect(prompt).toContain("&lt;tag&gt; &amp; keep **Markdown**.");
    expect(prompt).toContain("&lt;/message&gt;");
    expect(prompt.match(/goal_read/g)?.length).toBe(1);
    expect(prompt).toContain("rollout_read");
    expect(prompt).toContain("rollout_update");
    expect(prompt).toContain("current instructions and live state take precedence");
    expect(prompt.split("\n\n")[0]).not.toContain("\n");
    expect(prompt.split("\n\n").find(paragraph => paragraph.startsWith("This task also"))).not.toContain("\n");
    expect(prompt).toContain("<message>[2026-10-17T14:00:00.123Z]\nUse &lt;tag&gt; &amp; keep **Markdown**.</message>");
  });

  test("strict execute schema rejects arbitrary browser commands and oversized batches", () => {
    const valid = executeActionRequestSchema.parse({
      ...base,
      prompt: { kind: "offline_chat", batchId: "batch-1", messages: [{ id: "m-1", markdown: "Continue." }] },
    });
    expect(valid.prompt.kind).toBe("offline_chat");
    expect(() => executeActionRequestSchema.parse({ ...valid, javascript: "document.cookie" })).toThrow();
    expect(() => executeActionRequestSchema.parse({
      ...base,
      prompt: {
        kind: "offline_chat",
        batchId: "batch-many",
        messages: Array.from({ length: 129 }, (_, index) => ({ id: `m-${index}`, markdown: "x" })),
      },
    })).toThrow();
  });
});

test("events are strict, bounded, and enumerate the Rust surface", () => {
  expect(ACTION_EVENT_TYPES).toEqual([
    "queued", "waiting", "navigating", "composer_ready", "model_verified", "connector_verified",
    "send_accepted", "conversation_created", "status_delta", "commentary_delta", "answer_delta",
    "submission_complete", "generation_complete", "approval_required", "authentication_required", "rate_limited",
    "external_interference", "ambiguous_send", "failed",
  ]);
  expect(validateActionEvent({
    sequence: 7,
    actionId: "action-1",
    atMs: 17,
    type: "answer_delta",
    markdown: "Stable answer",
    continuation: false,
  }).type).toBe("answer_delta");
  expect(() => validateActionEvent({
    sequence: 7,
    actionId: "action-1",
    atMs: 17,
    type: "answer_delta",
    markdown: "x".repeat(65537),
    continuation: false,
  })).toThrow();
  expect(() => validateActionEvent({ sequence: 1, actionId: "a", atMs: 1, type: "queued", html: "<body>" })).toThrow();
});
