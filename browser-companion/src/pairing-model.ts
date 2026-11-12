import type { Page } from "patchright";
import { activeComposer } from "./composer";
import { BrowserCompanionError,browserStage } from "./errors";
import {browserEffortIdFromLabel } from "./models";
import {withChatGptModelOptions} from "./model-picker";
import {observeReadOnly} from "./observation";
import {
  requireChatGptEffortControl, activateChatGptEffortMenu, discoverChatGptEfforts,
  moveChatGptEffortSlider, readChatGptEffortSnapshot, readCurrentEffortLabel,
} from "./session";

export interface PairingMediumSelection {
  verify(signal: AbortSignal): Promise<void>;
  restore(signal: AbortSignal): Promise<void>;
}

const normalize = (label: string) => label.normalize("NFKC").replace(/\p{Cf}/gu, "").replace(/\s+/g, " ").trim();

/** The pairing turn borrows Medium; neither the model family nor saved preferences are changed. */
export async function selectPairingMedium(page: Page, signal: AbortSignal): Promise<PairingMediumSelection> {
  return browserStage("effort_selection","ChatGPT could not select and verify Medium for pairing",()=>selectPairingMediumInner(page,signal));
}

async function selectPairingMediumInner(page: Page, signal: AbortSignal): Promise<PairingMediumSelection> {
  const composer = await browserStage("composer_lookup","ChatGPT did not expose one editable pairing composer",()=>activeComposer(page, 10_000, signal));
  const control = await requireChatGptEffortControl(composer.locator("xpath=ancestor::form[1]"),signal);
  const activate=(operationSignal=signal)=>browserStage("picker_open","ChatGPT did not open its pairing effort picker",()=>activateChatGptEffortMenu(page,control,{signal:operationSignal}));
  const close=()=>control.press("Escape",{timeout:1_000}).catch(()=>{});
  const checkedFamily = async (menu: Awaited<ReturnType<typeof activateChatGptEffortMenu>>,operationSignal=signal) => {
    const {value:rows}=await withChatGptModelOptions(menu,()=>activate(operationSignal),async(_scope,rows)=>rows,{signal:operationSignal});
    const checked=rows.filter(row=>row.checked&&row.family);
    if(checked.length!==1||!normalize(checked[0]!.label))throw new BrowserCompanionError("ChatGPT did not identify the pairing request's model family", "model_unavailable",{phase:"model_identity"});
    return checked[0]!.family??normalize(checked[0]!.label);
  };
  let menu = await activate();
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
  const inspect = async (operationSignal=signal) => {
    menu = await activate(operationSignal);
    const state = await observeReadOnly(readChatGptEffortSnapshot(menu.sliderContainer),operationSignal);
    if (await checkedFamily(menu,operationSignal) !== family || state.min !== original.min || state.max !== original.max) {
      throw new BrowserCompanionError("ChatGPT changed the model family or effort range during pairing", "external_interference");
    }
    return {state, label:await observeReadOnly(readCurrentEffortLabel(menu),operationSignal)};
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
    return {
      verify:verifySignal=>browserStage("effort_selection","ChatGPT did not retain Medium for pairing",()=>verify(verifySignal)),
      restore:restoreSignal=>browserStage("effort_restore","ChatGPT could not restore the original pairing effort",()=>restore(restoreSignal)),
    };
  } catch (error) {
    await restore(AbortSignal.timeout(2_000));
    throw error;
  }
}
