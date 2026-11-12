import { expect, test } from "bun:test";
import { chromium } from "patchright";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { privateChromeExecutable } from "../src/chrome-executable";
import { buildCapabilityCatalog, browserEffortIdFromLabel } from "../src/models";
import { detectChatGptAccountCapabilities } from "../src/session";
import { selectModelAndEffort, assertSelectedModelAndEffort } from "../src/model-selection";

const fixture = `<!doctype html><style>#prompt-textarea{min-height:24px}[data-model-reasoning-effort-slider]{height:24px;min-width:240px}</style><form>
  <div id="prompt-textarea" contenteditable="true" role="textbox"></div>
  <button type="button" id="effort" data-testid="model-switcher-dropdown-button" aria-haspopup="menu" aria-controls="picker" aria-expanded="false">Pro</button>
</form><div id="picker" role="menu" hidden>
  <div data-model-picker-view="advanced">
    <button role="menuitemradio" data-family="6" aria-checked="true">GPT-6</button>
    <button role="menuitemradio" data-family="5.5" aria-checked="false">GPT-5.5Leaving on October 14</button>
  </div>
  <div id="range" role="menuitem" tabindex="0" aria-describedby="effort-description"></div>
  <span id="effort-description"></span>
</div><script>
 const definitions = {
  '6': {min:0, value:2, labels:['Instant','Deep','Pro'], locked:[1]},
  '5.5': {min:7, value:8, labels:['Turbo','Instant','Balanced','Deep','Sustained','Pro'], locked:[2]}
 };
 let family='6';
 const control=document.getElementById('effort'), picker=document.getElementById('picker'), range=document.getElementById('range');
 function render() {
  const model=definitions[family], index=model.value-model.min;
  range.innerHTML='<div data-model-reasoning-effort-slider><span role="slider" aria-valuemin="'+model.min+'" aria-valuemax="'+(model.min+model.labels.length-1)+'" aria-valuenow="'+model.value+'"></span>'+model.labels.map((label,i)=>'<span data-selected="'+(i===index)+'" data-locked="'+model.locked.includes(i)+'" aria-label="'+label+'"></span>').join('')+'</div>';
  document.getElementById('effort-description').textContent=document.body.hasAttribute('data-break-effort-announcement')?'Unknown effort':family+' '+model.labels[index]+', '+(index+1)+' of '+model.labels.length+'.';
  control.textContent=model.labels[index];
  document.querySelectorAll('[data-family]').forEach(row=>row.setAttribute('aria-checked',String(row.dataset.family===family)));
 }
 control.onclick=()=>{picker.hidden=false;control.setAttribute('aria-expanded','true');};
 document.querySelectorAll('[data-family]').forEach(row=>row.onclick=()=>{family=row.dataset.family;render();});
 range.onkeydown=event=>{
  if (!['ArrowLeft','ArrowRight'].includes(event.key)) return;
  event.preventDefault();const model=definitions[family], step=event.key==='ArrowRight'?1:-1;
  let next=model.value+step;
  while(next>=model.min&&next<model.min+model.labels.length&&model.locked.includes(next-model.min)) next+=step;
  if(next>=model.min&&next<model.min+model.labels.length) model.value=next;
  render();
 };
 document.addEventListener('keydown',event=>{if(event.key==='Escape'){picker.hidden=true;control.setAttribute('aria-expanded','false');}});
 render();
</script>`;

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME !== "1")(
  "Chrome discovers each family's native effort range, labels and locks and restores the original selection",
  async () => {
    const profilePath = await mkdtemp(join(tmpdir(), "native-effort-discovery-"));
    const executablePath = await privateChromeExecutable(join(import.meta.dirname, "../.browsers", `${process.platform}-${process.arch}`));
    const context = await chromium.launchPersistentContext(profilePath, {channel:"chrome",executablePath,headless:true,viewport:null});
    try {
      await context.route("https://chatgpt.com/**", route => route.fulfill({contentType:"text/html",body:fixture}));
      const page = context.pages()[0] ?? await context.newPage();
      await page.goto("https://chatgpt.com/");
      const capabilities = await detectChatGptAccountCapabilities(page);
      const catalog = buildCapabilityCatalog(capabilities, "fixture-account");
      expect(catalog.models.map(model => [model.label, model.efforts.map(effort => [effort.label, effort.available])])).toEqual([
        ["GPT-6 Astra", [["Pro", true]]],
        ["GPT-6", [["Instant", true], ["Deep", false]]],
        ["GPT-5.5", [["Turbo", true], ["Instant", true], ["Balanced", false], ["Deep", true], ["Sustained", true], ["Pro", true]]],
      ]);
      expect(await page.locator('[data-family="6"]').getAttribute("aria-checked")).toBe("true");
      expect(await page.locator('[role="slider"]').getAttribute("aria-valuenow")).toBe("2");
      expect(await page.locator("#effort").getAttribute("aria-expanded")).toBe("false");
      const selected = await selectModelAndEffort(page, "gpt-5.5-sol", browserEffortIdFromLabel("Deep"), capabilities);
      expect(selected.effortIndex).toBe(3);
      expect(await page.locator('[role="slider"]').getAttribute("aria-valuenow")).toBe("10");
      await assertSelectedModelAndEffort(page, selected);
      await page.evaluate(() => { document.body.setAttribute("data-break-effort-announcement", "true"); });
      let discoveryError: unknown;
      try { await detectChatGptAccountCapabilities(page); } catch (error) { discoveryError = error; }
      expect(discoveryError).toBeInstanceOf(Error);
      expect(discoveryError).toMatchObject({code:"selector_drift",phase:"effort_selection"});
      expect(await page.locator('[data-family="5.5"]').getAttribute("aria-checked")).toBe("true");
      expect(await page.locator('[role="slider"]').getAttribute("aria-valuenow")).toBe("10");
      expect(await page.locator("#effort").getAttribute("aria-expanded")).toBe("false");
      await page.setContent('<form><div id="prompt-textarea" style="min-height:24px" contenteditable="true" role="textbox"></div></form>');
      let missingControlError: unknown;
      try { await detectChatGptAccountCapabilities(page, {selectorTimeoutMs:0,stableAbsenceMs:0}); } catch (error) { missingControlError = error; }
      expect(missingControlError).toBeInstanceOf(Error);
      expect((missingControlError as Error).message).toContain("did not expose discoverable");
    } finally { await context.close(); await rm(profilePath, {recursive:true,force:true}); }
  }, 30_000,
);
