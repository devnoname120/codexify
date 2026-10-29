import type {Locator} from "patchright";
import {BrowserCompanionError,browserStage} from "./errors";
import {readModelOptionRows} from "./models";
import {readChatGptEffortSnapshot,type ChatGptEffortActivation} from "./session";

type Rows=Awaited<ReturnType<typeof readModelOptionRows>>;

/** Inspect the owned model list, including lists mounted only after expansion. */
export async function withChatGptModelOptions<T>(
  menu:ChatGptEffortActivation,activate:()=>Promise<ChatGptEffortActivation>,
  inspect:(scope:Locator,rows:Rows)=>Promise<T>,
  options:{signal?:AbortSignal;visible?:boolean;changesModel?:boolean}={},
):Promise<{menu:ChatGptEffortActivation;value:T}> {
  return browserStage("model_options","ChatGPT did not expose its model options",async()=>{
    options.signal?.throwIfAborted();
    let scope=menu.menu,rows=await readModelOptionRows(scope),expanded=false;
    let original:Awaited<ReturnType<typeof readChatGptEffortSnapshot>>|undefined;
    const usable=async()=>{
      if(!rows.some(row=>row.family))return false;
      if(!options.visible)return true;
      // The collapsed power picker keeps its rows mounted with layout boxes.
      // Playwright visibility alone does not exclude that inert, covered panel.
      if(await scope.locator('[data-model-picker-view="simple"]').count())return false;
      const active=scope.locator('[role="menuitemradio"][data-browser-model-family]').filter({visible:true});
      return await active.evaluateAll(elements=>elements.filter(element=>!element.closest('[inert], [aria-hidden="true"]')).length)
        ===rows.filter(row=>row.family).length;
    };
    try {
      if(!await usable()) {
        original=await readChatGptEffortSnapshot(menu.sliderContainer);
        const view=menu.menu.locator('[data-model-picker-view]');
        if(await view.count()>1)throw new BrowserCompanionError("ChatGPT exposed multiple model picker views","selector_drift");
        const toggle=menu.menu.locator('[data-model-picker-view-toggle="true"]:not([aria-hidden="true"])').filter({visible:true});
        const trigger=await toggle.count()?toggle:menu.menu.locator('[role="menuitem"][aria-expanded]:not([aria-hidden="true"])').filter({visible:true});
        if(await trigger.count()!==1)throw new BrowserCompanionError("ChatGPT model submenu control was missing or ambiguous","selector_drift");
        const state=await trigger.getAttribute("aria-expanded");
        // A portal can replace its trigger during the click. Capture ownership
        // while it is still mounted, rather than losing the submenu reference.
        let submenuId=await trigger.getAttribute("aria-controls");
        const simple=await view.count()===1&&await view.getAttribute("data-model-picker-view")==="simple";
        if(simple||state!=="true") {await trigger.click({timeout:5_000,signal:options.signal});expanded=true;}
        const deadline=Date.now()+5_000;
        do {
          options.signal?.throwIfAborted();
          // Some submenu triggers publish aria-controls only once opened. Read
          // it during hydration, rather than assuming the closed trigger has it.
          submenuId=await trigger.evaluateAll(nodes=>nodes.length===1?nodes[0]!.getAttribute("aria-controls"):null)??submenuId;
          if(submenuId)scope=menu.menu.page().locator(`[id=${JSON.stringify(submenuId)}]`);
          rows=await readModelOptionRows(scope);
          if(await usable())break;
          await new Promise(resolve=>setTimeout(resolve,50));
        }while(Date.now()<deadline);
        if(!await usable())throw new BrowserCompanionError("ChatGPT model submenu did not expose supported model rows","selector_drift");
      }
      const value=await inspect(scope,rows);
      return {menu,value};
    } finally {
      if(expanded) {
        // Escape changes only the view. Never select a model just to inspect it.
        await menu.menu.page().keyboard.press("Escape");
        const restored=await activate();
        Object.assign(menu,restored);
        if(original&&!options.changesModel) {
          const state=await readChatGptEffortSnapshot(menu.sliderContainer);
          if(state.min!==original.min||state.max!==original.max||state.value!==original.value) {
            throw new BrowserCompanionError("ChatGPT changed the effort while inspecting model options","external_interference");
          }
        }
      }
    }
  });
}
