// Derived from miuuyy/codex-chatgpt-web
// src/chatgpt-session.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
// Keep selector and verification changes structurally reviewable against upstream.
import type { Locator, Page } from "patchright";
import {observeReadOnly} from "./observation";
import {parseEffortAnnouncement,uiLabels} from "./ui-localization";
import { composerText } from "./composer";
import { browserEffortIdFromLabel, readModelOptionRows, modelsFromDiscoveredEfforts, type AccountCapabilities, type BrowserModelFamily, type DiscoveredBrowserEffort } from "./models";
import { selectChatGptModelFamily } from "./model-selection";

export const CHATGPT_TEMPORARY_CHAT_URL = "https://chatgpt.com/?temporary-chat=true";
export const CHATGPT_SAVED_CHAT_URL = "https://chatgpt.com/";

export function chatGptNewChatUrl(useSavedChats = false): string {
  return useSavedChats ? CHATGPT_SAVED_CHAT_URL : CHATGPT_TEMPORARY_CHAT_URL;
}
export const CHATGPT_COMPOSER_SELECTOR = [
  '[data-testid="prompt-textarea"]',
  "#prompt-textarea",
  '[contenteditable="true"][data-lexical-editor="true"]',
  'form[data-chatgpt-composer] [data-composer-markdown][contenteditable="true"][role="textbox"]',
].join(", ");
export const CHATGPT_EFFORT_CONTROL_SELECTOR = [
  'button[data-codexify-effort-control="true"][aria-haspopup="menu"]',
  'button[aria-haspopup="menu"][data-tone="neutral"]',
  'button[data-testid="model-switcher-dropdown-button"][aria-haspopup="menu"]',
  'button[data-composer-navigation-target="reasoning"][aria-haspopup="menu"]',
].join(", ");
export const CHATGPT_EFFORT_MENU_SELECTOR = [
  '[data-testid="composer-intelligence-picker-content"]:has([role="menuitemradio"], [data-model-reasoning-effort-slider])',
  '[role="menu"]:has([role="menuitemradio"], [data-model-reasoning-effort-slider])',
  '[role="group"]:has([role="menuitemradio"], [data-model-reasoning-effort-slider])',
  '[role="menu"]:has([data-model-picker-power-slider])',
].join(", ");
export const CHATGPT_EFFORT_ITEM_SELECTOR = '[role="menuitemradio"]';
export const CHATGPT_EFFORT_SLIDER_CONTAINER_SELECTOR = '[data-model-reasoning-effort-slider], [data-model-picker-power-slider]';
export const CHATGPT_EFFORT_SLIDER_SELECTOR = '[data-model-reasoning-effort-slider] [role="slider"], [data-model-picker-power-slider] [role="slider"]';
export const CHATGPT_EFFORT_SLIDER_MAX_OPTIONS = 16;

/** Model identity precedes the localized slider position; punctuation is not language-specific. */
export function parseChatGptModelAnnouncement(text: string): {
  version: string; name?: string; mode: string;
} | undefined {
  const normalized = text.normalize("NFKC").replace(/\p{Cf}/gu, "").replace(/\s+/g, " ").trim();
  const match = /^(?:GPT[-\s]?)?(\d+(?:\.\d+)?)(?:\s+(Sol|Luna|Astra))?\s+([^\p{P}]+)(?:\p{P}|$)/iu.exec(normalized);
  if (!match) return undefined;
  return { version: match[1]!, ...(match[2] ? { name: match[2].toLowerCase() } : {}), mode: match[3]!.trim() };
}

/** Read model evidence only from the slider's own active picker. */
export async function readChatGptModelAnnouncements(slider: Locator,signal?:AbortSignal): Promise<string[]> {
  return observeReadOnly(slider.evaluate(element => {
    const doc = element.ownerDocument;
    const descriptions = (element.closest('[role="menuitem"]')?.getAttribute("aria-describedby") ?? "")
      .split(/\s+/).filter(Boolean).map(id => doc.getElementById(id)?.textContent ?? "");
    // Some accounts now announce only the effort ("Pro, 5 of 5"); the model
    // version is shown in the picker header as adjacent text nodes ("6" + "Pro").
    const menu = element.closest('[role="menu"]');
    const rendered = (node: Element): boolean => {
      for (let parent: Element | null = node; parent; parent = parent.parentElement) {
        if (parent.matches('[hidden], [inert], [aria-hidden="true"]')) return false;
        const style = getComputedStyle(parent);
        if (style.display === "none" || style.visibility === "hidden" || style.opacity === "0") return false;
      }
      return true;
    };
    const headers = [...(menu?.querySelectorAll('[data-model-picker-view-toggle="true"]') ?? [])]
      .filter(header => header.closest('[role="menu"]') === menu && rendered(header));
    if (headers.length > 1) throw new Error("ChatGPT model picker exposes multiple active model headers");
    if (headers.length === 1) {
      const content = headers[0]!.querySelector("[data-menu-row-content]") ?? headers[0]!;
      const words: string[] = [];
      const walker = doc.createTreeWalker(content, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        if (node.parentElement && rendered(node.parentElement)) {
          const word = node.textContent?.trim();
          if (word) words.push(word);
        }
      }
      descriptions.push(words.join(" "));
    }
    return descriptions;
  }),signal);
}
/** Resolve only inside the verified composer's form; multiple submitters are an error. */
export const CHATGPT_SEND_BUTTON_SELECTOR = '[data-testid="send-button"]:not(:has(svg use[href*="#voice-"])), button[type="submit"]:not(:has(svg use[href*="#voice-"]))';
export const CHATGPT_STOP_BUTTON_SELECTOR = '[data-testid="stop-button"], [data-testid="end-turn-button"], button[aria-label="End turn"], form[data-chatgpt-composer] button[type="button"]:is('+uiLabels('stopControl').map(label=>'[aria-label='+JSON.stringify(label)+']').join(',')+')';
// The new footer is shared with user messages. Response extraction additionally requires
// this control to FOLLOW the last assistant answer, excluding the user's earlier footer.
export const CHATGPT_COMPLETION_ACTION_SELECTOR = 'button[data-testid="copy-turn-action-button"], [data-turn-key] .turn-action-controls button';
export const CHATGPT_ASSISTANT_TURN_SELECTOR = [
  '[data-testid^="conversation-turn-"][data-turn="assistant"]:not([data-turn-key] *)',
  '[data-testid^="conversation-turn-"][data-message-author-role="assistant"]:not([data-turn-key] *)',
  '[data-testid^="conversation-turn-"]:has([data-message-author-role="assistant"]):not([data-turn-key] *)',
  '[data-turn-key]:has([data-conversation-role="assistant"], [data-chatgpt-agent-turn-start])',
].join(", ");
export const CHATGPT_USER_TURN_SELECTOR = [
  '[data-testid^="conversation-turn-"][data-turn="user"]:not([data-turn-key] *)',
  '[data-testid^="conversation-turn-"][data-message-author-role="user"]:not([data-turn-key] *)',
  '[data-testid^="conversation-turn-"]:has([data-message-author-role="user"]):not([data-turn-key] *)',
  '[data-turn-key]:has([data-user-message-bubble])',
].join(", ");

/** The new renderer groups both roles under the user's stable turn key. */
export function chatGptAssistantTurnSelector(identity: string): string {
  const prefix = "group:assistant:";
  return identity.startsWith(prefix)
    ? `[data-turn-key=${JSON.stringify(identity.slice(prefix.length))}]:has([data-conversation-role="assistant"], [data-chatgpt-agent-turn-start])`
    : `[data-turn-id=${JSON.stringify(identity)}]`;
}

export interface ChatGptEffortSliderState {
  min: number;
  max: number;
  value: number;
}

export interface ChatGptEffortActivation {
  method: "already-open" | "click" | "pointerdown";
  menu: Locator;
  sliderContainer: Locator;
  slider: Locator;
}

export function chatGptEffortSlider(page: Page): { sliderContainer: Locator; slider: Locator } {
  const sliderContainer = page.locator(CHATGPT_EFFORT_SLIDER_CONTAINER_SELECTOR).filter({ visible: true });
  // The current picker keeps ARIA values on a zero-width, aria-hidden semantic input.
  // Its visible container proves the active surface; the input proves the effort range.
  return { sliderContainer, slider: sliderContainer.locator('[role="slider"]') };
}

function effortMenuSelectorForId(menuId: string): string {
  return `[id=${JSON.stringify(menuId)}]`;
}

export async function chatGptEffortMenuForControl(page: Page, control: Locator): Promise<Locator> {
  const menuId = await control.getAttribute("aria-controls",{timeout:500}).catch(() => null);
  if (menuId) return page.locator(effortMenuSelectorForId(menuId));
  const controlId = await control.getAttribute("id",{timeout:500}).catch(() => null);
  if (controlId) return page.locator(`[role="menu"][aria-labelledby~=${JSON.stringify(controlId)}]`).filter({ visible: true });
  return page.locator(CHATGPT_EFFORT_MENU_SELECTOR).filter({ visible: true });
}

async function visibleEffortSurface(
  page: Page,
  control: Locator,
): Promise<Omit<ChatGptEffortActivation, "method"> | undefined> {
  // The exit animation keeps a closed menu's slider visible after Escape. Read the
  // owner state first: selecting that outgoing range races its removal from the DOM.
  const expanded = await control.getAttribute("aria-expanded",{timeout:500}).catch(() => null);
  const state = await control.getAttribute("data-state",{timeout:500}).catch(() => null);
  if (expanded === "false" || state === "closed") return undefined;
  const menu = await chatGptEffortMenuForControl(page, control);
  if (!await menu.isVisible().catch(() => false)) return undefined;
  const sliderContainer = menu.locator(CHATGPT_EFFORT_SLIDER_CONTAINER_SELECTOR).filter({ visible:true });
  if (!await sliderContainer.isVisible().catch(() => false)) return undefined;
  return { menu, sliderContainer, slider:sliderContainer.locator('[role="slider"]') };
}

async function waitForEffortSurface(
  page: Page,
  control: Locator,
  timeoutMs: number,
  signal?:AbortSignal,
): Promise<Omit<ChatGptEffortActivation, "method"> | undefined> {
  const deadline = Date.now() + timeoutMs;
  do {
    signal?.throwIfAborted();
    const surface = await observeReadOnly(visibleEffortSurface(page, control),signal);
    if (surface) return surface;
    if (Date.now() >= deadline) return undefined;
    await new Promise(resolveSleep => setTimeout(resolveSleep, 50));
  } while (true);
}

async function clearGhostEffortState(page: Page, control: Locator): Promise<void> {
  const expanded = await control.getAttribute("aria-expanded",{timeout:500}).catch(() => null);
  const state = await control.getAttribute("data-state",{timeout:500}).catch(() => null);
  if (expanded === "true" || state === "open") {
    await control.press("Escape",{timeout:1_000}).catch(() => {});
  }
}

export async function activateChatGptEffortMenu(
  page: Page,
  control: Locator,
  options: { settleMs?: number; signal?:AbortSignal } = {},
): Promise<ChatGptEffortActivation> {
  options.signal?.throwIfAborted();
  // Presentation attributes can change while opening the picker. Keep the
  // verified control addressable without relying on its closed-state styling.
  await control.evaluate(element=>element.setAttribute("data-codexify-effort-control","true"),undefined,{timeout:1_500});
  options.signal?.throwIfAborted();
  const openSurface = await observeReadOnly(visibleEffortSurface(page, control),options.signal);
  if (openSurface) return { method: "already-open", ...openSurface };

  const settleMs = options.settleMs ?? 3_000;
  await clearGhostEffortState(page, control);
  options.signal?.throwIfAborted();
  await control.click({ force: true, timeout: Math.max(1, settleMs),signal:options.signal });
  const clickedSurface = await waitForEffortSurface(page, control, settleMs,options.signal);
  if (clickedSurface) return { method: "click", ...clickedSurface };

  await clearGhostEffortState(page, control);
  options.signal?.throwIfAborted();
  await control.dispatchEvent("pointerdown", {
    button: 0,
    buttons: 1,
    pointerType: "mouse",
    isPrimary: true,
  },{timeout:Math.max(1,settleMs)});
  const pointerSurface = await waitForEffortSurface(page, control, settleMs,options.signal);
  if (pointerSurface) return { method: "pointerdown", ...pointerSurface };
  throw new Error(
    "ChatGPT effort control did not expose its owned menu or structural slider after click and primary pointerdown",
  );
}

function safeIntegerAttribute(value: string | null): number | undefined {
  if (value === null || !/^-?\d+$/.test(value)) return undefined;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

export function parseChatGptEffortSliderState(
  rawMin: string | null,
  rawMax: string | null,
  rawValue: string | null,
): ChatGptEffortSliderState | undefined {
  const min = safeIntegerAttribute(rawMin);
  const max = safeIntegerAttribute(rawMax);
  const value = safeIntegerAttribute(rawValue);
  if (min === undefined || max === undefined || value === undefined) return undefined;
  const optionCount = max - min + 1;
  if (optionCount < 1 || optionCount > CHATGPT_EFFORT_SLIDER_MAX_OPTIONS) return undefined;
  if (value < min || value > max) return undefined;
  return { min, max, value };
}

export async function readChatGptEffortSnapshot(
  sliderContainer: Locator,
  timeoutMs = 1_000,
  signal?:AbortSignal,
): Promise<ChatGptEffortSliderState & { available: boolean[]; labels: (string | null)[] }> {
  const deadline = Date.now() + timeoutMs;
  do {
    // Read the range, selection and locks in one DOM revision. Separate Playwright
    // reads can straddle hydration and combine a five-step range with four ticks.
    const snapshot = await observeReadOnly(sliderContainer.evaluate(container => {
      const sliders = container.querySelectorAll('[role="slider"]');
      const slider = sliders.length === 1 ? sliders[0] : undefined;
      const power = container.hasAttribute("data-model-picker-power-slider")
        && Boolean(container.querySelector('[data-orientation="horizontal"][aria-disabled="false"]'));
      return {
        min: slider?.getAttribute("aria-valuemin") ?? null,
        max: slider?.getAttribute("aria-valuemax") ?? null,
        value: slider?.getAttribute("aria-valuenow") ?? null,
        locks: Array.from(container.querySelectorAll("[data-selected]"), tick =>
          tick.getAttribute("data-locked") ?? (power ? "false" : null)),
        labels: Array.from(container.querySelectorAll("[data-selected]"), tick =>
          (tick.getAttribute("aria-label") ?? tick.getAttribute("title") ?? tick.textContent)?.trim() || null),
      };
    }),signal);
    const state = parseChatGptEffortSliderState(snapshot.min, snapshot.max, snapshot.value);
    if (!state) throw new Error("ChatGPT effort slider exposed an invalid ARIA range");
    if (snapshot.locks.some(lock => lock !== "true" && lock !== "false")) break;
    if (snapshot.locks.length === state.max - state.min + 1) {
      return { ...state, available: snapshot.locks.map(lock => lock === "false"), labels:snapshot.labels };
    }
    if (Date.now() >= deadline) break;
    await new Promise(resolve => setTimeout(resolve, 50));
  } while (true);
  throw new Error("ChatGPT effort availability could not be verified from its slider ticks");
}

export function chatGptEffortLabelFromAnnouncement(text: string, state: ChatGptEffortSliderState): string | undefined {
  const announcement=parseEffortAnnouncement(text);
  if(!announcement||announcement.position!==state.value-state.min+1||announcement.total!==state.max-state.min+1)return undefined;
  return parseChatGptModelAnnouncement(announcement.label)?.mode??announcement.label;
}

export async function readCurrentEffortLabel(menu: ChatGptEffortActivation,signal?:AbortSignal): Promise<string> {
  const deadline = Date.now() + 1_000;
  do {
    const state = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal);
    const valueText = await observeReadOnly(menu.slider.getAttribute("aria-valuetext",{timeout:1_500}),signal);
    if (valueText?.trim()) {
      const counted = chatGptEffortLabelFromAnnouncement(valueText, state);
      if (counted) return counted;
      if (!parseEffortAnnouncement(valueText)) {
        return parseChatGptModelAnnouncement(valueText)?.mode ?? valueText.trim();
      }
    }
    const labels = (await readChatGptModelAnnouncements(menu.slider,signal))
      .map(text => chatGptEffortLabelFromAnnouncement(text, state)).filter((label): label is string => Boolean(label));
    if (new Set(labels).size === 1) return labels[0]!;
    if (Date.now() >= deadline) break;
    await new Promise(resolve => setTimeout(resolve, 50));
  } while (true);
  throw new Error("ChatGPT did not announce the selected effort's label and position");
}

export async function moveChatGptEffortSlider(
  menu: ChatGptEffortActivation,
  target: number,
  signal?: AbortSignal,
): Promise<void> {
  let state = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal);
  const { min, max } = state;
  if (target < min || target > max || !state.available[target - min]) throw new Error("ChatGPT locks the requested effort");
  const control = menu.slider.locator("xpath=ancestor::*[@role='menuitem'][1]");
  while (state.value !== target) {
    if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
    const direction = Math.sign(target - state.value);
    const before = state.value;
    await control.press(direction > 0 ? "ArrowRight" : "ArrowLeft", { timeout:5_000, signal });
    const deadline = Date.now() + 5_000;
    do {
      state = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal);
      if (state.min !== min || state.max !== max || !state.available[target - min]) throw new Error("ChatGPT changed its effort range or availability");
      if (state.value !== before) break;
      await new Promise(resolve => setTimeout(resolve, 50));
    } while (Date.now() < deadline);
    if ((state.value - before) * direction <= 0 || (state.value - target) * direction > 0) {
      throw new Error("ChatGPT did not move its effort slider toward the requested position");
    }
  }
}

export async function discoverChatGptEfforts(menu: ChatGptEffortActivation, signal?:AbortSignal): Promise<DiscoveredBrowserEffort[]> {
  const initial = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal);
  const efforts: DiscoveredBrowserEffort[] = [];
  try {
    for (let index = 0; index < initial.available.length; index += 1) {
      signal?.throwIfAborted();
      const available = initial.available[index]!;
      let label = initial.labels[index];
      if (available) {
        await moveChatGptEffortSlider(menu, initial.min + index,signal);
        label = await readCurrentEffortLabel(menu,signal);
      }
      // An unlabeled locked tick does not establish an effort name. Do not invent one.
      if (!label) continue;
      const id = browserEffortIdFromLabel(label);
      if (efforts.some(effort => effort.id === id)) throw new Error("ChatGPT exposed duplicate effort labels");
      const observedModelName=available?(await readChatGptModelAnnouncements(menu.slider,signal)).map(parseChatGptModelAnnouncement).find(model=>model?.name)?.name:undefined;
      efforts.push({ id, label, index, available, reason:available ? null : "not_available_for_account",...(observedModelName?{observedModelName}:{}) });
    }
  } finally {
    // The caller may have released its operation deadline. Never start a new
    // picker mutation after cancellation, including a well-intended restore.
    if(!signal?.aborted)await moveChatGptEffortSlider(menu, initial.value,signal);
  }
  if (efforts.length === 0) throw new Error("ChatGPT did not expose any discoverable effort labels");
  return efforts;
}

async function anyVisible(locator: Locator): Promise<boolean> {
  const count = await locator.count();
  for (let index = 0; index < count; index += 1) {
    if (await locator.nth(index).isVisible().catch(() => false)) return true;
  }
  return false;
}

export async function assertAuthenticatedChatGptPage(page: Page): Promise<void> {
  const composer = page.locator(
    CHATGPT_COMPOSER_SELECTOR,
  );
  if (!await anyVisible(composer)) {
    throw new Error("ChatGPT authentication could not be verified: no visible composer is present");
  }
}

export async function assertTemporaryChatPage(page: Page): Promise<void> {
  await assertNewChatPage(page);
}

export async function assertNewChatPage(page: Page, useSavedChats = false): Promise<void> {
  const url = new URL(page.url());
  const expected = new URL(chatGptNewChatUrl(useSavedChats));
  if (url.origin !== expected.origin || url.pathname !== expected.pathname
    || (url.searchParams.get("temporary-chat") === "true") === useSavedChats) {
    throw new Error(`ChatGPT left the requested new ${useSavedChats ? "saved" : "Temporary"} Chat surface (${page.url()})`);
  }
}

export async function detectChatGptAccountCapabilities(
  page: Page,
  options: { selectorTimeoutMs?: number; stableAbsenceMs?: number; signal?:AbortSignal; onProgress?:(step:string)=>void } = {},
): Promise<AccountCapabilities> {
  options.signal?.throwIfAborted();
  options.onProgress?.("waiting for effort control");
  const composers = page.locator(CHATGPT_COMPOSER_SELECTOR).filter({ visible: true });
  const composer = composers;
  const composerForm = composer.locator("xpath=ancestor::form[1]");
  const effortButton = composerForm.locator(CHATGPT_EFFORT_CONTROL_SELECTOR).filter({ visible: true });
  const deadline = Date.now() + (options.selectorTimeoutMs ?? 30_000);
  const stableAbsenceMs = options.stableAbsenceMs ?? 3_000;
  let absenceSince: number | undefined;
  let presenceObservations = 0;
  while (true) {
    options.signal?.throwIfAborted();
    const effortVisible = await observeReadOnly(effortButton.isVisible(),options.signal);
    if (effortVisible) {
      presenceObservations += 1;
      absenceSince = undefined;
      if (presenceObservations >= 2) break;
      await new Promise(resolveSleep => setTimeout(resolveSleep, 100));
      continue;
    }
    presenceObservations = 0;
    const composerReady = await observeReadOnly(composers.count(),options.signal).then(count => count === 1);
    const formReady = await observeReadOnly(composerForm.count(),options.signal).then(count => count === 1);
    const documentReady = await observeReadOnly(page.evaluate(() => document.readyState === "complete"),options.signal);
    if (composerReady && formReady && documentReady) {
      absenceSince ??= Date.now();
    } else {
      absenceSince = undefined;
    }
    if (Date.now() >= deadline) {
      // ChatGPT can mount a usable composer before the account's model list arrives.
      // A short absence is not a capability result; use the complete inspection budget.
      if (absenceSince !== undefined && Date.now() - absenceSince >= stableAbsenceMs) {
        throw new Error("ChatGPT did not expose discoverable model and effort controls");
      }
      throw new Error("ChatGPT account capability probe did not reach a stable composer state");
    }
    await new Promise(resolveSleep => setTimeout(resolveSleep, 100));
  }
  if ((await observeReadOnly(composerText(composer),options.signal)).trim()) throw new Error("ChatGPT capability discovery requires an empty composer");
  options.onProgress?.("opening model picker");
  let menu = await activateChatGptEffortMenu(page, effortButton,{signal:options.signal});
  let originalFamily: BrowserModelFamily | undefined;
  let originalValue: number | undefined;
  try {
    options.onProgress?.("reading models");
    const rows = await observeReadOnly(readModelOptionRows(menu.menu),options.signal);
    const modelFamilies = [...new Set(rows.flatMap(row=>row.family?[row.family]:[]))];
    if (modelFamilies.length === 0) throw new Error("ChatGPT model picker did not expose a supported model family");
    const checked=rows.filter(row=>row.checked);
    originalFamily=checked.length===1?checked[0]!.family:undefined;
    if (!originalFamily) throw new Error("ChatGPT did not identify its selected model family");
    originalValue = (await readChatGptEffortSnapshot(menu.sliderContainer,1_000,options.signal)).value;
    const models: AccountCapabilities["models"][number][] = [];
    for (const family of modelFamilies) {
      options.signal?.throwIfAborted();
      options.onProgress?.(`reading efforts ${family}`);
      menu = await selectChatGptModelFamily(menu, family, () => activateChatGptEffortMenu(page, effortButton,{signal:options.signal}),options.signal);
      const familyLabel=(await observeReadOnly(readModelOptionRows(menu.menu),options.signal)).find(row=>row.family===family)?.label;
      models.push(...modelsFromDiscoveredEfforts(family, await discoverChatGptEfforts(menu,options.signal),familyLabel));
    }
    return { models };
  } finally {
    try {
      if (!options.signal?.aborted && originalFamily !== undefined && originalValue !== undefined) {
        options.onProgress?.("restoring picker");
        menu = await selectChatGptModelFamily(menu, originalFamily, () => activateChatGptEffortMenu(page, effortButton,{signal:options.signal}),options.signal);
        await moveChatGptEffortSlider(menu, originalValue,options.signal);
      }
    } finally {
      if(!options.signal?.aborted)await effortButton.press("Escape",{timeout:1_000,signal:options.signal}).catch(() => {});
    }
  }
}
