// Derived from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/model-selection.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import { activateChatGptEffortMenu, parseChatGptEffortSliderState, parseChatGptModelAnnouncement, readChatGptModelAnnouncements } from "./session";
import type { Locator, Page } from "patchright";
import {uiLabelPattern,parseEffortAnnouncement} from "./ui-localization";
import { activeComposer, composerText } from "./composer";
import { readChatGptEffortSnapshot, readCurrentEffortLabel, moveChatGptEffortSlider, CHATGPT_EFFORT_CONTROL_SELECTOR } from "./session";
import { readModelOptionRows, browserEffortIdFromLabel, modelsFromDiscoveredEfforts, resolveBrowserModel, type AccountCapabilities, type BrowserEffort, type BrowserModelFamily, type ResolvedBrowserModel } from "./models";
import { BrowserCompanionError, ModelOptionUnavailableError } from "./errors";
import { canonicalChatGptConversationUrl } from "./protocol";
import { modelFallbacks } from "./model-fallback";
import {observeReadOnly} from "./observation";

type EffortMenu = Awaited<ReturnType<typeof activateChatGptEffortMenu>>;

function familyError(family: BrowserModelFamily, cause?: unknown): BrowserCompanionError {
  return new BrowserCompanionError(
    `ChatGPT model ${family} could not be selected and verified. The pending message was not sent. Check the model in the browser; refresh the model catalog and retry.`,
    "model_unavailable",
    { cause },
  );
}

async function familyOption(menu: EffortMenu, family: BrowserModelFamily,signal?:AbortSignal) {
  await observeReadOnly(readModelOptionRows(menu.menu),signal);
  return menu.menu.locator(`[role="menuitemradio"][data-browser-model-family="${family}"]`);
}

/** Model and effort are separate browser controls; a generic Pro label proves neither family. */
export async function selectChatGptModelFamily(
  menu: EffortMenu,
  family: BrowserModelFamily,
  activate: () => Promise<EffortMenu>,
  signal?:AbortSignal,
): Promise<EffortMenu> {
  try {
    signal?.throwIfAborted();
    const option = await familyOption(menu, family,signal);
    if (await option.count() > 1) throw familyError(family);
    if (await option.count() === 1 && (await option.getAttribute("aria-disabled") === "true" || await option.getAttribute("disabled") !== null)) {
      throw new ModelOptionUnavailableError(`ChatGPT model ${family} is unavailable`,family);
    }
    if (await option.count() === 1 && await option.getAttribute("aria-checked") === "true") return menu;
    // The attached radio rows are inert while this composer-owned advanced view is collapsed.
    const powerView = menu.menu.locator('[data-model-picker-view]');
    if (await powerView.count() === 1) {
      const view = await powerView.getAttribute("data-model-picker-view");
      if (view === "simple") {
        const trigger = powerView.locator('[data-model-picker-view-toggle="true"]:not([aria-hidden="true"])')
          .filter({ visible: true });
        if (await trigger.count() !== 1) throw familyError(family);
        await trigger.click({ timeout: 5_000,signal });
      } else if (view !== "advanced") throw familyError(family);
    } else {
      const trigger = menu.menu.locator('[role="menuitem"][aria-expanded][aria-hidden="false"]');
      if (await powerView.count() !== 0 || await trigger.count() !== 1) throw familyError(family);
      if (await trigger.getAttribute("aria-expanded") === "false") await trigger.click({ timeout: 5_000,signal });
    }
    if (await option.count() === 0) {
      const rows=await observeReadOnly(readModelOptionRows(menu.menu),signal);
      if (rows.some(row=>row.family)) {
        throw new ModelOptionUnavailableError(`ChatGPT model ${family} is not offered in this conversation`,family);
      }
      throw familyError(family);
    }
    await option.waitFor({ state: "visible", timeout: 5_000,signal });
    await option.click({ timeout: 5_000,signal });
    // Choosing a family returns the open picker to its slider. Keep that surface:
    // Escape followed by an immediate reopen races the outgoing menu's cleanup.
    // Activation reuses the open menu and verifies its owner before returning it.
    const selected = await activate();
    const deadline = Date.now() + 1_000;
    do {
      signal?.throwIfAborted();
      const current = await familyOption(selected, family,signal);
      if (await current.count() > 1) throw familyError(family);
      if (await current.count() === 1 && await current.getAttribute("aria-checked") === "true") return selected;
      await new Promise(resolve => setTimeout(resolve, 50));
    } while (Date.now() < deadline);
    throw familyError(family);
  } catch (cause) {
    if (cause instanceof BrowserCompanionError) throw cause;
    throw familyError(family, cause);
  }
}

export function chatGptModelFamilyMatches(
  descriptions: readonly string[],
  family: BrowserModelFamily,
  effort: BrowserEffort,
  observedName?: string,
): boolean {
  // GPT-6 uses Sol below Pro and Astra at Pro. An old Latest picker can still use
  // 5.6 at lower efforts; that is not proof of an explicitly requested GPT-6 turn.
  const expectedName = family === "6" && effort === "max" ? "astra" : "sol";
  const states = descriptions.flatMap(text => {
    const state = parseChatGptModelAnnouncement(text);
    return state ? [state] : [];
  });
  return states.length > 0 && states.every(state => state.version === family
    && (!state.name || state.name === expectedName || state.name === observedName)
    && browserEffortIdFromLabel(state.mode) === effort);
}

export function chatGptModelSelectionMatches(
  descriptions: readonly string[],
  family: BrowserModelFamily,
  effort: BrowserEffort,
  familyChecked: boolean,
  familyLabel: string | null,
): boolean {
  if (!familyChecked) return false;
  // The current picker announces only the effort. Its checked family radio is
  // the model evidence; retain explicit version announcements when present.
  const hasVersion = descriptions.some(text => parseChatGptModelAnnouncement(text) !== undefined);
  const selectedVersion = /^(?:GPT[-\s]?)?(\d+(?:\.\d+)?)(?=\s|$)/i
    .exec(familyLabel?.normalize("NFKC").trim() ?? "")?.[1];
  if (!hasVersion && selectedVersion !== family) return false;
  const evidence = hasVersion ? descriptions : descriptions
    .filter(text => { const label = parseEffortAnnouncement(text)?.label ?? text.trim(); return label && browserEffortIdFromLabel(label) === effort; })
    .map(text => `${family} ${text}`);
  const observedName=/\b(Sol|Luna|Astra)\b/i.exec(familyLabel||"")?.[1]?.toLowerCase();
  return chatGptModelFamilyMatches(evidence, family, effort, observedName);
}

export async function assertChatGptModelFamily(
  menu: EffortMenu,
  family: BrowserModelFamily,
  effort: BrowserEffort,
  effortIndex: number,
  settleMs = 0,
  signal?:AbortSignal,
): Promise<void> {
  const deadline = Date.now() + settleMs;
  do {
    signal?.throwIfAborted();
    const option = await familyOption(menu, family,signal);
    const checked = await option.count() === 1 && await option.getAttribute("aria-checked") === "true";
    const state = parseChatGptEffortSliderState(
      await menu.slider.getAttribute("aria-valuemin"), await menu.slider.getAttribute("aria-valuemax"),
      await menu.slider.getAttribute("aria-valuenow"),
    );
    const descriptions = await readChatGptModelAnnouncements(menu.slider,signal);
    const familyLabel = checked ? (await observeReadOnly(readModelOptionRows(menu.menu),signal)).find(row=>row.checked&&row.family===family)?.label??null : null;
    if (state && state.value === state.min + effortIndex && chatGptModelSelectionMatches(descriptions, family, effort, checked, familyLabel)) return;
    if (Date.now() >= deadline) break;
    await new Promise(resolve => setTimeout(resolve, 50));
  } while (true);
  throw familyError(family);
}

export interface SelectedBrowserModel extends ResolvedBrowserModel {
  selection: { url:string; label:string; origin?:number } | null;
}

async function thinkControls(form:Locator):Promise<Locator> {
  const semantic=form.locator('button[data-testid="think-button"],button[data-composer-navigation-target="reasoning"][aria-pressed]').filter({visible:true});
  return await semantic.count()?semantic:form.getByRole("button",{name:uiLabelPattern("thinkControl"),exact:true}).filter({visible:true});
}

async function setLunaThinkMode(
  composer: Locator,
  enabled: boolean,
  signal?: AbortSignal,
): Promise<void> {
  const form = composer.locator("xpath=ancestor::form[1]");
  const controls = await thinkControls(form);
  const count = await controls.count();
  if (count > 1) throw new BrowserCompanionError("ChatGPT exposed multiple Think controls", "model_unavailable");
  const action = { timeout:10_000, signal };
  const control = controls.first();
  let pressed = count === 1 ? await control.getAttribute("aria-pressed", action) : null;
  const target = enabled ? "true" : "false";
  if (count === 0 && !enabled) return;
  if (count === 1 && pressed !== "true" && pressed !== "false") {
    throw new BrowserCompanionError("ChatGPT Think control has no semantic state", "model_unavailable");
  }
  if ((await composerText(composer)).trim()) {
    throw new BrowserCompanionError("ChatGPT Think selection requires an empty composer", "external_interference");
  }
  if (pressed !== target) {
    if (count === 1) {
      await control.click(action);
    } else {
      await composer.focus(action);
      await composer.press("End", action);
      await composer.pressSequentially("/think", { ...action, delay:25 });
      const popup = form.page().locator('.popover[aria-busy="false"]').filter({ visible:true });
      const rows = popup.locator('.__menu-item[tabindex="0"]').filter({ visible:true });
      await rows.first().waitFor({ state:"visible", timeout:5_000, signal });
      if (await popup.count() !== 1 || await rows.count() !== 1) {
        throw new BrowserCompanionError("ChatGPT Think command is unavailable", "model_unavailable");
      }
      const row = rows.first();
      if (await row.getAttribute("data-highlighted", action) === null) await composer.press("ArrowDown", action);
      if (await row.getAttribute("data-highlighted", action) === null) {
        throw new BrowserCompanionError("ChatGPT Think command cannot be selected", "model_unavailable");
      }
      await composer.press("Enter", action);
    }
    const deadline = Date.now() + 5_000;
    while (Date.now() < deadline) {
      if (signal?.aborted) throw new DOMException("Browser action aborted", "AbortError");
      const current = await thinkControls(form);
      const currentCount = await current.count();
      if (currentCount > 1) throw new BrowserCompanionError("ChatGPT exposed multiple Think controls", "model_unavailable");
      pressed = currentCount === 1 ? await current.first().getAttribute("aria-pressed", action) : null;
      if (pressed === target) break;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    if (pressed !== target) throw new BrowserCompanionError("ChatGPT did not retain the requested Think mode", "model_unavailable");
  }
  if ((await composerText(composer)).trim()) {
    throw new BrowserCompanionError("ChatGPT Think selection mutated the prompt draft", "external_interference");
  }
}

async function availableEffort(menu: EffortMenu, index: number,signal?:AbortSignal): Promise<Awaited<ReturnType<typeof readChatGptEffortSnapshot>>> {
  const state = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal).catch(error => {
    throw new BrowserCompanionError(
      "ChatGPT effort availability could not be verified",
      "model_unavailable",
      { cause:error },
    );
  });
  if (index > state.max - state.min) {
    throw new BrowserCompanionError(`ChatGPT effort slider does not expose index ${index}`, "model_unavailable");
  }
  if (!state.available[index]) {
    throw new ModelOptionUnavailableError("ChatGPT locks the requested effort for this account");
  }
  return state;
}

/** Resolve only the requested effort on the selected destination family. */
async function requestedEffort(menu: EffortMenu, effort: string, cachedIndex: number | null | undefined, signal?: AbortSignal) {
  const state = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal);
  const matches = state.labels.flatMap((label, index) => label && browserEffortIdFromLabel(label) === effort ? [index] : []);
  if (matches.length > 1) throw new Error("ChatGPT exposed duplicate effort labels");
  const direct = matches[0];
  if (direct !== undefined) {
    if (!state.available[direct]) throw new ModelOptionUnavailableError("ChatGPT locks the requested effort");
    return {id:effort, label:state.labels[direct]!, index:direct, available:true, reason:null};
  }
  // Some picker versions label only the selected tick. Use a cached position as
  // a hint, verifying its live announcement; never treat the cache as authority.
  const currentLabel = await readCurrentEffortLabel(menu,signal);
  if (browserEffortIdFromLabel(currentLabel) === effort) {
    if (!state.available[state.value-state.min]) throw new ModelOptionUnavailableError("ChatGPT locks the requested effort");
    return {id:effort, label:currentLabel, index:state.value - state.min, available:true, reason:null};
  }
  const candidates = new Set<number>();
  if (cachedIndex != null && state.available[cachedIndex] && !state.labels[cachedIndex]) candidates.add(cachedIndex);
  state.labels.forEach((label, index) => { if (!label && state.available[index]) candidates.add(index); });
  for (const index of candidates) {
    if (state.min + index === state.value) continue;
    await moveChatGptEffortSlider(menu, state.min + index, signal);
    const live = await readChatGptEffortSnapshot(menu.sliderContainer,1_000,signal);
    if (live.min !== state.min || live.max !== state.max) throw new Error("ChatGPT changed its effort range");
    const label = await readCurrentEffortLabel(menu,signal);
    if (browserEffortIdFromLabel(label) === effort) return {id:effort, label, index, available:true, reason:null};
  }
  throw new ModelOptionUnavailableError("ChatGPT did not expose the requested effort");
}

export async function selectModelAndEffort(
  page: Page,
  model: string,
  effort: string,
  capabilities?: AccountCapabilities,
  signal?: AbortSignal,
  deferConfirmation=false,
): Promise<SelectedBrowserModel> {
  const family = /^gpt-(5\.5|5\.6|6)-(?:sol|astra)$/.exec(model)?.[1] as BrowserModelFamily | undefined;
  const luna = model === "gpt-5.6-luna" && capabilities ? resolveBrowserModel(model, effort, capabilities) : null;
  if (!luna && (!family || (model.endsWith("-astra") && (family !== "6" || effort !== "max"))
    || (model === "gpt-6-sol" && effort === "max"))) {
    throw new BrowserCompanionError("The requested ChatGPT model and effort are unsupported", "model_unavailable");
  }
  const composer = await activeComposer(page, 30_000, signal);
  const form = composer.locator("xpath=ancestor::form[1]");
  const controls = form.locator(CHATGPT_EFFORT_CONTROL_SELECTOR).filter({ visible:true });
  if (luna) {
    if (await controls.count() > 0) {
      throw new BrowserCompanionError("ChatGPT account changed from Luna-only to Sol during selection", "model_unavailable");
    }
    await setLunaThinkMode(composer, luna.lunaThink, signal);
    return { ...luna, selection:null };
  }
  try {await controls.first().waitFor({ state:"visible", timeout:8_000, signal });}
  catch(error) {
    signal?.throwIfAborted();
    throw new BrowserCompanionError("ChatGPT's model control did not finish loading; nothing was sent","model_unavailable",{cause:error});
  }
  if (await controls.count() !== 1) {
    throw new BrowserCompanionError("ChatGPT did not expose one model/effort control", "model_unavailable");
  }
  const control = controls.first();
  let menu = await activateChatGptEffortMenu(page, control,{signal});
  menu = await selectChatGptModelFamily(menu, family!, () => activateChatGptEffortMenu(page, control,{signal}),signal);
  let resolved: ResolvedBrowserModel;
  try {
    const option = await requestedEffort(menu, effort, capabilities?.models.find(candidate => candidate.id === model)
      ?.efforts.find(candidate => candidate.id === effort)?.index, signal);
    resolved = resolveBrowserModel(model, effort, {models:modelsFromDiscoveredEfforts(family!, [option])});
  } catch (error) {
    await page.keyboard.press("Escape").catch(() => {});
    signal?.throwIfAborted();
    if (error instanceof ModelOptionUnavailableError) throw error;
    throw new BrowserCompanionError(`The requested ChatGPT model and effort could not be selected in this conversation: ${error instanceof Error ? error.message : "verification failed"}`, "model_unavailable", {cause:error});
  }
  const effortIndex = resolved.effortIndex;
  if (effortIndex === null) throw new BrowserCompanionError("ChatGPT did not identify the requested effort position", "model_unavailable");
  const slider = await availableEffort(menu, effortIndex,signal);
  const origin = slider.min;
  const target = origin + effortIndex;
  await moveChatGptEffortSlider(menu, target, signal);
  const selectedUrl = page.url();
  await page.keyboard.press("Escape");
  const label = (await control.innerText({timeout:1_500}).catch(()=>"")).trim();
  signal?.throwIfAborted();
  if (!label || await control.getAttribute("aria-expanded",{timeout:500}) !== "false") {
    throw new BrowserCompanionError("ChatGPT did not close the model picker with a selected effort", "model_unavailable");
  }
  if(!deferConfirmation){
    const confirmation = await activateChatGptEffortMenu(page, control,{signal});
    const confirmed = await availableEffort(confirmation, effortIndex,signal);
    if (confirmed.min !== origin || confirmed.value !== target) {
      throw new BrowserCompanionError("ChatGPT did not persist the requested effort", "model_unavailable");
    }
    await assertChatGptModelFamily(confirmation, resolved.family!, resolved.effort, effortIndex, 1_000,signal);
    await page.keyboard.press("Escape");
  }
  return { ...resolved, selection:{ url:selectedUrl, label, origin } };
}

export async function selectAvailableModelAndEffort(
  page:Page,model:string,effort:string,capabilities?:AccountCapabilities,signal?:AbortSignal,
  verifyFallbackDestination?:()=>Promise<void>,
  deferConfirmation=false,
):Promise<SelectedBrowserModel> {
  const missingFamilies=new Set<string>();
  const candidates=[{model,effort},...modelFallbacks(model,effort)];
  for (let index=0;index<candidates.length;index++) {
    const candidate=candidates[index]!;
    const family=/^gpt-([\d.]+)-/.exec(candidate.model)?.[1];
    if (family&&missingFamilies.has(family)&&candidate.model!=="gpt-5.6-luna") continue;
    if (candidate.model==="gpt-5.6-luna"&&!capabilities?.models.some(option=>option.id===candidate.model)) continue;
    signal?.throwIfAborted();
    if (index>0) { await verifyFallbackDestination?.(); await page.keyboard.press("Escape"); }
    try { return await selectModelAndEffort(page,candidate.model,candidate.effort,capabilities,signal,deferConfirmation); }
    catch(error) {
      signal?.throwIfAborted();
      if (!(error instanceof ModelOptionUnavailableError)) throw error;
      if (error.unavailableFamily) missingFamilies.add(error.unavailableFamily);
    }
  }
  await page.keyboard.press("Escape").catch(()=>{});
  throw new ModelOptionUnavailableError("The preferred ChatGPT model and all supported fallbacks are unavailable; the message was not sent");
}

export async function assertSelectedModelAndEffort(
  page: Page,
  selected: SelectedBrowserModel,
  signal?: AbortSignal,
  newChat = false,
): Promise<void> {
  const composer = await activeComposer(page, 30_000, signal);
  if (selected.effortIndex === null) {
    await setLunaThinkMode(composer, selected.lunaThink, signal);
    return;
  }
  if (!selected.selection || (!newChat && page.url() !== selected.selection.url
    && canonicalChatGptConversationUrl(page.url()) !== canonicalChatGptConversationUrl(selected.selection.url))) {
    throw new BrowserCompanionError("ChatGPT changed the selected conversation before Send", "external_interference");
  }
  const controls = composer.locator("xpath=ancestor::form[1]")
    .locator(CHATGPT_EFFORT_CONTROL_SELECTOR).filter({ visible:true });
  if (await controls.count() !== 1) throw new BrowserCompanionError("ChatGPT model control changed before Send", "model_unavailable");
  const control = controls.first();
  if ((await control.innerText()).trim() !== selected.selection.label
    || await control.getAttribute("aria-expanded") !== "false") {
    throw new BrowserCompanionError("ChatGPT did not retain the selected model and effort", "external_interference");
  }
  const menu = await activateChatGptEffortMenu(page, control,{signal});
  try {
    const confirmed=await availableEffort(menu, selected.effortIndex,signal);
    if(confirmed.value!==confirmed.min+selected.effortIndex
      ||selected.selection.origin!==undefined&&confirmed.min!==selected.selection.origin)
      throw new BrowserCompanionError("ChatGPT did not retain the requested effort before Send","model_unavailable");
    await assertChatGptModelFamily(menu, selected.family!, selected.effort, selected.effortIndex,0,signal);
  } finally {
    await page.keyboard.press("Escape");
  }
}
