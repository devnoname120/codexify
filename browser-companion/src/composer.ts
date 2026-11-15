import {observeReadOnly} from "./observation";
// Derived from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/browser-worker.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
// Exact insertion semantics remain structurally aligned with upstream.
import type { Locator, Page } from "patchright";
import { CHATGPT_COMPOSER_SELECTOR, CHATGPT_SEND_BUTTON_SELECTOR } from "./session";
import { renderActionPrompt, rolloutRestoreGuidance } from "./protocol";
import { BrowserCompanionError } from "./errors";

export const COMPOSER_READY_TIMEOUT_MS = 30_000;

export function insertPlainTextIntoComposer(element: HTMLElement, value: string): boolean {
  if (document.activeElement !== element) element.focus();
  if (document.activeElement !== element) return false;
  const selection = window.getSelection();
  if (!selection) return false;
  const alreadyPlaced = selection.isCollapsed
    && selection.anchorNode !== null
    && element.contains(selection.anchorNode);
  if (!alreadyPlaced) {
    const range = document.createRange();
    range.selectNodeContents(element);
    range.collapse(false);
    selection.removeAllRanges();
    selection.addRange(range);
  }
  if (!selection.isCollapsed || !selection.anchorNode || !element.contains(selection.anchorNode)) {
    return false;
  }
  return document.execCommand("insertText", false, value);
}

export function verifyOwnedComposerText(actual: string, expected: string): boolean {
  return expected.length > 0 && actual === expected;
}

/** Only fixed goal-control prose, never queued user messages or edited drafts. */
export function isAutomationGoalDraft(text:string):boolean {
  const normalize=(value:string)=>value.replace(/\s+/g,' ').trim();
  const legacyStart='This task also has an active Codexify goal. Call `goal_read` for the current revision. If it is complete, call `goal_finish` with status `completed`. If work ';
  const legacyEnd='Otherwise continue working toward the goal. Do not request a separate turn merely to evaluate goal status.';
  const variants=[
    renderActionPrompt({kind:'offline_goal',goalRevision:1}),
    legacyStart+'cannot proceed, call `goal_finish` with status `blocked` and explain the concrete blocker. '+legacyEnd+'\n\n'+rolloutRestoreGuidance,
    legacyStart+'cannot proceed because of a verified external dependency, first check alternatives and record evidence and the exact input needed. Infinite goals require the same blocker to persist across three distinct turns before `goal_finish` accepts status `blocked`. Retry temporary browser/tool errors, continue independent work, and restore progress with `rollout_read` after context loss. '+legacyEnd+'\n\n'+rolloutRestoreGuidance,
  ];
  return variants.some(value=>normalize(value)===normalize(text));
}

export function isPairingVerificationDraft(text: string): boolean {
  const reference = text.match(/[\n ]calling `automation_verify_pairing` with the non-secret: ([A-Za-z0-9_-]+)[\n ]/)?.[1];
  if (!reference) return false;
  try {
    const current = renderActionPrompt({ kind:"pair_verify", reference });
    // An interrupted pairing can leave the exact pre-update, hard-wrapped draft.
    const legacy = current.replace("task by calling", "task by\ncalling")
      .replace(`${reference} Do not inspect`, `${reference}\nDo not inspect`);
    return text === current || text === legacy;
  } catch {
    return false;
  }
}

export async function activeComposer(
  page: Page,
  timeoutMs = COMPOSER_READY_TIMEOUT_MS,
  signal?: AbortSignal,
): Promise<Locator> {
  const composers = page.locator(CHATGPT_COMPOSER_SELECTOR).filter({ visible:true });
  const deadline = Date.now() + timeoutMs;
  let count = 0;
  while (Date.now() < deadline) {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    const observationBudget=Math.max(1,Math.min(5000,deadline-Date.now()));
    try {
      count = await observeReadOnly(composers.count(),signal,observationBudget);
    } catch(error) {
      signal?.throwIfAborted();
      // Readiness uses short probes while the editor hydrates. Exhausting that
      // caller's sub-five-second budget is a missing editor for THIS probe,
      // not a stalled renderer. Let readiness retry within its outer deadline.
      // A full observation timeout and cancellation still propagate unchanged.
      if(timeoutMs<5000 && observationBudget<5000 && Date.now()>=deadline
        && error instanceof BrowserCompanionError && error.code==="editor_unavailable")break;
      throw error;
    }
    if (count === 1) {
      const composer = composers.first();
      if (await observeReadOnly(composer.isEditable(),signal,Math.max(1,Math.min(5_000,deadline-Date.now()))).catch(error=>{signal?.throwIfAborted();return false;})) return composer;
    }
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error(`ChatGPT composer is unavailable or ambiguous (visible count=${count})`);
}

export async function composerText(composer: Locator): Promise<string> {
  return observeReadOnly(composer.evaluate(element => {
    const root = element as HTMLElement;
    if(/^(TEXTAREA|INPUT)$/.test(root.tagName))return (root as HTMLTextAreaElement).value;
    // Project the live editor's logical visible text, retaining paragraph
    // boundaries while excluding app chips and hidden editor scaffolding.
    const ignored=(node:Node):boolean=>{
      if(node.nodeType!==1||node===root)return false;
      const element=node as HTMLElement;
      if(element.matches('[contenteditable="false"], [app-mention-path], [data-id^="plugin:"], [hidden], [inert], [aria-hidden="true"]'))return true;
      const style=element.ownerDocument.defaultView?.getComputedStyle?.(element);
      return !!style && (style.display==='none'||style.visibility==='hidden'||style.visibility==='collapse'||style.opacity==='0');
    };
    const text = (node: Node): string => {
      if (node.nodeType === 3) return node.textContent ?? "";
      if(ignored(node))return '';
      if (node.nodeType === 1 && (node as Element).tagName === "BR") {
        const parent = node.parentElement;
        const emptyParagraph = parent && /^(P|DIV)$/.test(parent.tagName) && parent.childNodes.length === 1;
        return (node as Element).classList.contains("ProseMirror-trailingBreak") || emptyParagraph ? "" : "\n";
      }
      let result = "";
      let previousBlock = false;
      let hasPrevious = false;
      for (const child of node.childNodes) {
        if(ignored(child))continue;
        const block = child.nodeType === 1 && /^(P|DIV)$/.test((child as Element).tagName);
        if (hasPrevious && (previousBlock || block)) result += "\n";
        result += text(child);
        previousBlock = block;
        hasPrevious = true;
      }
      return result;
    };
    return text(root).replace(/^[\s\u200b\u200c\u200d\u2060\ufeff]+/, "");
  }));
}

export async function ensureEmptyOwnedComposer(composer: Locator): Promise<void> {
  const text = await composerText(composer);
  if (text.trim().length !== 0) {
    throw new BrowserCompanionError("ChatGPT composer contains an existing draft; automation will not overwrite it", "external_interference");
  }
}

export async function insertExactPrompt(composer: Locator, prompt: string): Promise<void> {
  await ensureEmptyOwnedComposer(composer);
  const inserted = await composer.evaluate(insertPlainTextIntoComposer, prompt);
  if (!inserted) throw new BrowserCompanionError("ChatGPT did not accept the message in its editor; nothing was sent", "editor_unavailable");
  const actual = await composerText(composer);
  if (!verifyOwnedComposerText(actual, prompt)) {
    throw new BrowserCompanionError("ChatGPT composer text changed during insertion; the message was not sent", "external_interference");
  }
}

export async function uniqueSendButton(
  composer: Locator,
  timeoutMs = COMPOSER_READY_TIMEOUT_MS,
  signal?: AbortSignal,
  checkBlocker?: () => Promise<void>,
): Promise<Locator> {
  signal?.throwIfAborted();
  const deadline = Date.now() + timeoutMs;
  const observe = <T>(pending:Promise<T>) => observeReadOnly(pending,signal,Math.max(1,Math.min(5_000,deadline-Date.now())));
  const form = composer.locator("xpath=ancestor::form[1]");
  if (await observe(form.count()) !== 1) throw new BrowserCompanionError("ChatGPT composer does not have one owned form", "selector_drift");
  const send = form.locator(CHATGPT_SEND_BUTTON_SELECTOR).filter({ visible:true });
  while (Date.now() < deadline) {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    if (checkBlocker) await observe(checkBlocker());
    const count = await observe(send.count());
    if (count > 1) throw new BrowserCompanionError("ChatGPT composer does not have one visible Send control", "selector_drift");
    if (count === 1 && await observe(send.first().isEnabled()).catch(error=>{signal?.throwIfAborted();return false;})) return send.first();
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new BrowserCompanionError("ChatGPT Send control did not become ready; the message was not sent", "selector_drift");
}
