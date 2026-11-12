import type { Page } from "patchright";
import { activeComposer } from "./composer";
import { BrowserCompanionError } from "./errors";
import {observeReadOnly} from "./observation";
import { readModelOptionRows,browserEffortIdFromLabel } from "./models";
import {
  CHATGPT_EFFORT_CONTROL_SELECTOR, activateChatGptEffortMenu,
  moveChatGptEffortSlider, readChatGptEffortSnapshot, readCurrentEffortLabel,
} from "./session";

export interface PairingMediumSelection {
  verify(signal: AbortSignal): Promise<void>;
  restore(signal: AbortSignal): Promise<void>;
}

const normalize = (label: string) => label.normalize("NFKC").replace(/\p{Cf}/gu, "").replace(/\s+/g, " ").trim();

/** The pairing turn borrows Medium; neither the model family nor saved preferences are changed. */
export async function selectPairingMedium(page: Page, signal: AbortSignal): Promise<PairingMediumSelection> {
  const composer = await activeComposer(page, 10_000, signal);
  const controls = composer.locator("xpath=ancestor::form[1]")
    .locator(CHATGPT_EFFORT_CONTROL_SELECTOR).filter({ visible: true });
  await controls.first().waitFor({state:"visible", timeout:10_000, signal});
  if (await observeReadOnly(controls.count(),signal) !== 1) throw new BrowserCompanionError("Pairing requires one verifiable Medium control; the request was not sent", "model_unavailable");
  const control = controls.first();
  const close=()=>control.press("Escape",{timeout:1_000}).catch(()=>{});
  const checkedFamily = async (menu: Awaited<ReturnType<typeof activateChatGptEffortMenu>>,readSignal:AbortSignal) => {
    const checked=(await observeReadOnly(readModelOptionRows(menu.menu),readSignal)).filter(row=>row.checked);
    if(checked.length!==1||!normalize(checked[0]!.label))throw new BrowserCompanionError("ChatGPT did not identify the pairing request's model family", "model_unavailable");
    return checked[0]!.family??normalize(checked[0]!.label);
  };
  let menu = await activateChatGptEffortMenu(page, control,{signal});
  let original: Awaited<ReturnType<typeof readChatGptEffortSnapshot>>;
  let family: string;
  let label: string;
  try {
    original = await observeReadOnly(readChatGptEffortSnapshot(menu.sliderContainer),signal);
    family = await checkedFamily(menu,signal);
    label = await observeReadOnly(readCurrentEffortLabel(menu),signal);
  } catch (error) { await close(); throw error; }
  let target: number | undefined;
  let mediumSelected = false;
  const inspect = async (readSignal:AbortSignal) => {
    menu = await activateChatGptEffortMenu(page, control,{signal:readSignal});
    const state = await observeReadOnly(readChatGptEffortSnapshot(menu.sliderContainer),readSignal);
    if (await checkedFamily(menu,readSignal) !== family || state.min !== original.min || state.max !== original.max) {
      throw new BrowserCompanionError("ChatGPT changed the model family or effort range during pairing", "external_interference");
    }
    return {state, label:await observeReadOnly(readCurrentEffortLabel(menu),readSignal)};
  };
  const restore = async (restoreSignal: AbortSignal) => {
    try {
      const current = await inspect(restoreSignal);
      if (current.state.value === original.value && normalize(current.label) === normalize(label)) return;
      if (mediumSelected && (current.state.value !== target || browserEffortIdFromLabel(current.label) !== "medium")) {
        throw new BrowserCompanionError("ChatGPT changed the temporary pairing selection; it was not overwritten", "external_interference");
      }
      await moveChatGptEffortSlider(menu, original.value, restoreSignal);
      const restored = await inspect(restoreSignal);
      if (restored.state.value !== original.value || normalize(restored.label) !== normalize(label)) {
        throw new BrowserCompanionError("ChatGPT did not restore the conversation's model and effort after pairing", "model_unavailable");
      }
    } finally { await close(); }
  };
  try {
    let index = original.labels.findIndex(value => value && browserEffortIdFromLabel(value) === "medium");
    if(index<0&&browserEffortIdFromLabel(label)==="medium")index=original.value-original.min;
    if (index < 0) {
      // Pairing needs one live Medium tick, not an exhaustive model catalogue.
      // Probe only unnamed available ticks and stop as soon as Medium is proven.
      const candidates=original.available.flatMap((available,index)=>available&&!original.labels[index]?[index]:[])
        .sort((left,right)=>Math.abs(original.min+left-original.value)-Math.abs(original.min+right-original.value));
      for(const candidate of candidates) {
        signal.throwIfAborted();
        if(!original.available[candidate]||original.labels[candidate]||original.min+candidate===original.value)continue;
        await moveChatGptEffortSlider(menu,original.min+candidate,signal);
        const current=await inspect(signal);
        if(browserEffortIdFromLabel(current.label)==="medium"){index=candidate;break;}
      }
    }
    if (index < 0 || !original.available[index]) {
      throw new BrowserCompanionError("Medium mode is unavailable for pairing; the pairing request was not sent", "model_unavailable");
    }
    target = original.min + index;
    await moveChatGptEffortSlider(menu, target, signal);
    const verify = async (verifySignal: AbortSignal) => {
      if (verifySignal.aborted) throw new DOMException("Browser action aborted", "AbortError");
      try {
        const current = await inspect(verifySignal);
        if (current.state.value !== target || browserEffortIdFromLabel(current.label) !== "medium") {
          throw new BrowserCompanionError("ChatGPT did not retain Medium mode for the pairing request", "model_unavailable");
        }
      } finally { await close(); }
    };
    await verify(signal);
    mediumSelected = true;
    return {verify, restore};
  } catch (error) {
    await restore(AbortSignal.timeout(2_000));
    throw error;
  }
}
