// Derived from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/browser-worker.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import type { Locator, Page } from "patchright";
import {selectorDrift} from "./errors";
import {observeReadOnly} from "./observation";
import {throwIfConversationLimit} from "./browser-session";
import {
  CHATGPT_ASSISTANT_TURN_SELECTOR,
  CHATGPT_STOP_BUTTON_SELECTOR,
  CHATGPT_USER_TURN_SELECTOR,
  chatGptAssistantTurnSelector,
} from "./session";

export type TurnRole = "user" | "assistant";

export interface TurnDomState {
  turnIdentities: string[];
  userIdentities: string[];
  responseIdentities: string[];
  generationRunning: boolean;
}
export interface TurnDeltaState {
  newUserIdentities: string[];
  newResponseIdentities: string[];
  generationRunning: boolean;
}

export interface SubmissionBaseline {
  initialTurnIdentities: readonly string[];
  submittedText: string;
  acceptedUserIdentity?: string;
  browserBaselineId?: number;
}

export interface AssistantTurnBinding {
  identity: string;
  locator: Locator;
  acceptedTurnIdentities: readonly string[];
}

export function turnIdentityFromAttributes(input: {
  role: TurnRole;
  turnKey: string | null;
  turnId: string | null;
}): string {
  if (input.turnKey?.trim()) return `group:${input.role}:${input.turnKey}`;
  if (input.turnId?.trim()) return input.turnId;
  throw new Error("ChatGPT conversation turn has no stable identity");
}

export function newTurnIdentity(initial: readonly string[], current: readonly string[]): string | undefined {
  const previous = new Set(initial);
  const added = [...new Set(current)].filter(identity => !previous.has(identity));
  if (added.length > 1) {
    throw new Error(`ChatGPT exposed ${added.length} new conversation turns for one submitted message`);
  }
  return added[0];
}

export function reboundTurnIdentity(
  initial: readonly string[],
  boundIdentity: string,
  current: readonly string[],
): string | undefined {
  if (current.includes(boundIdentity)) return boundIdentity;
  return newTurnIdentity(initial, current);
}

export type SubmissionEvidence = "user_turn" | "assistant_turn" | "generation_running";

export function submissionEvidence(state: {
  initialTurnIdentities: readonly string[];
  userIdentities: readonly string[];
  responseIdentities: readonly string[];
  generationRunning: boolean;
}): SubmissionEvidence | undefined {
  if (newTurnIdentity(state.initialTurnIdentities, state.userIdentities)) return "user_turn";
  if (newTurnIdentity(state.initialTurnIdentities, state.responseIdentities)) return "assistant_turn";
  if (state.generationRunning) return "generation_running";
  return undefined;
}

function unique(values: string[], kind: string): string[] {
  if (values.some(value => value.trim().length === 0)) {
    throw selectorDrift(`ChatGPT conversation ${kind} has an empty stable identity`);
  }
  // Responsive views and remount transitions may render more than one physical
  // node for the same logical turn. Stable IDs, rather than node counts, bind Send.
  return [...new Set(values)];
}

export function readTurnDomState(page: Page, browserBaselineId?: undefined, signal?: AbortSignal, timeoutMs?: number): Promise<TurnDomState>;
export function readTurnDomState(page: Page, browserBaselineId: number, signal?: AbortSignal, timeoutMs?: number): Promise<TurnDeltaState>;
export async function readTurnDomState(page: Page, browserBaselineId?: number, signal?: AbortSignal, timeoutMs=5_000): Promise<TurnDomState | TurnDeltaState> {
  signal?.throwIfAborted();
  return observeReadOnly(page.evaluate(({ userSelector, assistantSelector, stopSelector, baselineId }) => {
    type TurnIdentityCache = {
      url: string;
      dirty: boolean;
      scans: number;
      state: Pick<TurnDomState, "turnIdentities" | "userIdentities" | "responseIdentities"> | null;
      observer: MutationObserver;
      nextBaselineId: number;
      baseline?: { id:number; known:Set<string> };
    };
    const scope = globalThis as typeof globalThis & { __CODEXIFY_TURN_IDENTITIES__?: TurnIdentityCache };
    let cache = scope.__CODEXIFY_TURN_IDENTITIES__;
    if (!cache) {
      const relevant = '[data-turn-key], [data-turn-id], [data-turn-id-container], [data-testid^="conversation-turn-"], [data-message-author-role], [data-user-message-bubble], [data-conversation-role], [data-chatgpt-agent-turn-start]';
      cache = {
        url: "", dirty: true, scans: 0, state: null, nextBaselineId: 0,
        observer: undefined as unknown as MutationObserver,
      };
      const current = cache;
      current.observer = new MutationObserver(records => {
        for (const record of records) {
          if (record.type === "attributes") {
            // Composer/status/test-id changes elsewhere on the page are not
            // new turns. Streaming UI changes must not rescan all old turns.
            if (record.attributeName === "data-testid"
              && !(record.oldValue ?? "").startsWith("conversation-turn-")
              && !(record.target as Element).matches(relevant)) continue;
            current.dirty = true;
            break;
          }
          // Streamed answer text is not a new conversation turn. Avoid a full
          // history scan for each Markdown/text update in very long chats.
          const changed = [...record.addedNodes, ...record.removedNodes].some(node =>
            node.nodeType === Node.ELEMENT_NODE
            && ((node as Element).matches(relevant) || (node as Element).querySelector(relevant) !== null));
          if (changed) {
            current.dirty = true;
            break;
          }
        }
      });
      current.observer.observe(document.documentElement, {
        childList: true, subtree: true, attributes: true,
        attributeOldValue: true,
        attributeFilter: [
          "data-turn-key", "data-turn-id", "data-turn-id-container", "data-testid",
          "data-message-author-role", "data-user-message-bubble",
          "data-conversation-role", "data-chatgpt-agent-turn-start", "data-turn",
        ],
      });
      scope.__CODEXIFY_TURN_IDENTITIES__ = current;
    }
    if (cache.dirty || cache.url !== location.href || cache.state === null) {
    if (cache.url !== location.href) cache.baseline = undefined;
    const legacy = (selector: string) => [...document.querySelectorAll<HTMLElement>(selector)]
      .filter(element => !element.closest("[data-turn-key]"));
    const legacyIdentity = (element: HTMLElement): string => {
      const id = element.getAttribute("data-turn-id");
      if (!id?.trim()) throw new Error("ChatGPT legacy turn has no stable data-turn-id");
      return id;
    };
    const userIdentities = legacy(userSelector).map(legacyIdentity);
    const responseIdentities = legacy(assistantSelector).map(legacyIdentity);
    const turnIdentities = [...document.querySelectorAll<HTMLElement>("[data-turn-id-container]")]
      .filter(element => !element.closest("[data-turn-key]")
        && element.parentElement?.closest("[data-turn-id-container]")?.getAttribute("data-turn-id-container")
          !== element.getAttribute("data-turn-id-container"))
      .map(element => {
        const id = element.getAttribute("data-turn-id-container");
        if (!id?.trim()) throw new Error("ChatGPT turn container has no stable identity");
        return id;
      });
    for (const group of document.querySelectorAll<HTMLElement>("[data-turn-key]")) {
      const key = group.getAttribute("data-turn-key");
      if (!key?.trim()) throw new Error("ChatGPT grouped turn has no stable data-turn-key");
      const user = `group:user:${key}`;
      const assistant = `group:assistant:${key}`;
      // Grouped user shells can exist before an assistant response is mounted.
      // Do not include an absent assistant in the Send baseline, or the
      // response later appearing under this same key looks like an old turn.
      if (group.querySelector("[data-user-message-bubble]")) {
        turnIdentities.push(user);
        userIdentities.push(user);
      }
      if (group.querySelector('[data-conversation-role="assistant"], [data-chatgpt-agent-turn-start]')) {
        turnIdentities.push(assistant);
        responseIdentities.push(assistant);
      }
    }
      cache.state = { turnIdentities, userIdentities, responseIdentities };
      cache.url = location.href;
      cache.scans++;
      cache.dirty = false;
    }
    const visible = (element: Element): boolean => {
      const style = getComputedStyle(element as HTMLElement);
      return (element as HTMLElement).isConnected
        && style.visibility !== "hidden"
        && style.display !== "none";
    };
    const generationRunning = [...document.querySelectorAll(stopSelector)].some(visible);
    if (baselineId !== undefined) {
      const known = cache.baseline?.id === baselineId ? cache.baseline.known : undefined;
      if (!known) throw new Error("ChatGPT conversation baseline changed during the action");
      return {
        newUserIdentities:cache.state.userIdentities.filter(id => !known.has(id)),
        newResponseIdentities:cache.state.responseIdentities.filter(id => !known.has(id)),
        generationRunning,
      };
    }
    return { ...cache.state, generationRunning };
  }, {
    userSelector:CHATGPT_USER_TURN_SELECTOR,
    assistantSelector:CHATGPT_ASSISTANT_TURN_SELECTOR,
    stopSelector:CHATGPT_STOP_BUTTON_SELECTOR,
    baselineId:browserBaselineId,
  }).catch(error=>{
    const reasons=["ChatGPT legacy turn has no stable data-turn-id","ChatGPT turn container has no stable identity","ChatGPT grouped turn has no stable data-turn-key"];
    const detail=error instanceof Error?reasons.find(reason=>error.message.includes(reason)):undefined;
    throw selectorDrift(detail??"ChatGPT conversation history could not be inspected before sending",error);
  }).then(state => baselineIdIsPresent(browserBaselineId) ? {
    newUserIdentities:unique((state as TurnDeltaState).newUserIdentities, "new user turn"),
    newResponseIdentities:unique((state as TurnDeltaState).newResponseIdentities, "new assistant turn"),
    generationRunning:state.generationRunning,
  } : {
    turnIdentities:unique((state as TurnDomState).turnIdentities, "turn"),
    userIdentities:unique((state as TurnDomState).userIdentities, "user turn"),
    responseIdentities:unique((state as TurnDomState).responseIdentities, "assistant turn"),
    generationRunning:state.generationRunning,
  }), signal, timeoutMs);
}

function baselineIdIsPresent(value: number | undefined): value is number {
  return value !== undefined;
}

export async function captureSubmissionBaseline(page: Page, submittedText: string, signal?: AbortSignal): Promise<SubmissionBaseline> {
  if (!submittedText) throw new Error("Submitted prompt must not be empty");
  const initialTurnIdentities = (await readTurnDomState(page, undefined, signal)).turnIdentities;
  signal?.throwIfAborted();
  const browserBaselineId = await observeReadOnly(page.evaluate(known => {
    const cache = (globalThis as any).__CODEXIFY_TURN_IDENTITIES__;
    if (!cache) throw new Error("ChatGPT conversation turn cache is missing");
    const id = ++cache.nextBaselineId;
    cache.baseline = { id, known:new Set(known) };
    return id as number;
  },initialTurnIdentities), signal);
  return { initialTurnIdentities, browserBaselineId, submittedText };
}

async function submissionPoll(page: Page, baseline: SubmissionBaseline, signal?: AbortSignal, timeoutMs?: number) {
  if (baseline.browserBaselineId === undefined) return readTurnDomState(page, undefined, signal, timeoutMs);
  const state = await readTurnDomState(page, baseline.browserBaselineId, signal, timeoutMs);
  return {
    userIdentities:state.newUserIdentities,
    responseIdentities:state.newResponseIdentities,
    generationRunning:state.generationRunning,
  };
}

export async function waitForSubmissionAcceptance(
  page: Page,
  baseline: SubmissionBaseline,
  deadline: number,
  signal?: AbortSignal,
): Promise<SubmissionEvidence> {
  while (Date.now() < deadline) {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    const state = await submissionPoll(page, baseline, signal, Math.max(1, Math.min(5_000, deadline-Date.now())));
    await throwIfConversationLimit(page);
    const evidence = submissionEvidence({ ...state, initialTurnIdentities:baseline.initialTurnIdentities });
    if (evidence === "user_turn") {
      const identity = newTurnIdentity(baseline.initialTurnIdentities, state.userIdentities)!;
      if (baseline.acceptedUserIdentity && baseline.acceptedUserIdentity !== identity) {
        throw new Error("ChatGPT changed the user turn that acknowledged Send");
      }
      baseline.acceptedUserIdentity = identity;
    }
    if (evidence) return evidence;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error("ChatGPT Send did not produce semantic acceptance evidence");
}

export async function waitForAssistantTurn(
  page: Page,
  baseline: SubmissionBaseline,
  deadline: number,
  signal?: AbortSignal,
): Promise<AssistantTurnBinding> {
  while (Date.now() < deadline) {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    const state = await submissionPoll(page, baseline, signal, Math.max(1, Math.min(5_000, deadline-Date.now())));
    const identity = newTurnIdentity(baseline.initialTurnIdentities, state.responseIdentities);
    if (identity) {
      const accepted = await readTurnDomState(page, undefined, signal);
      if (newTurnIdentity(baseline.initialTurnIdentities, accepted.responseIdentities) !== identity) {
        throw new Error("ChatGPT changed assistant turns while binding the accepted response");
      }
      return {
        identity,
        locator:page.locator(chatGptAssistantTurnSelector(identity)),
        acceptedTurnIdentities:accepted.turnIdentities,
      };
    }
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error("ChatGPT accepted the message but exposed no assistant turn");
}

export async function reconcileAssistantTurn(
  page: Page,
  baseline: SubmissionBaseline,
  binding: AssistantTurnBinding,
): Promise<AssistantTurnBinding> {
  const count = await binding.locator.count();
  if (count === 1) return binding;
  if (count > 1) throw new Error("ChatGPT exposed duplicate nodes for the bound assistant turn");
  const state = await readTurnDomState(page);
  const accepted = new Set([...binding.acceptedTurnIdentities, ...baseline.initialTurnIdentities]);
  const newUsers = state.userIdentities.filter(identity => !accepted.has(identity));
  if (newUsers.length > 0) {
    throw new Error("ChatGPT opened another user turn while the bound assistant response was detached");
  }
  const identity = reboundTurnIdentity(baseline.initialTurnIdentities, binding.identity, state.responseIdentities);
  if (!identity || identity === binding.identity) return binding;
  return {
    identity,
    locator:page.locator(chatGptAssistantTurnSelector(identity)),
    acceptedTurnIdentities:state.turnIdentities,
  };
}
