// Derived from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/browser-worker.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import type { Locator, Page } from "patchright";
import { activeComposer, composerText } from "./composer";
import { BrowserCompanionError } from "./errors";

const CONNECTOR_ACTION_TIMEOUT_MS = 5_000;
// Leave the query intact while the app menu loads. Clearing/retyping every few
// seconds restarts React's search and can keep a slow menu from ever appearing.
// The caller can refresh the empty editor once and retry the stable query.
const CONNECTOR_MENU_TIMEOUT_MS = 8_000;
const MAX_CONNECTOR_TRIGGER_ATTEMPTS = 1;
const CONNECTOR_COMPOSER_TIMEOUT_MS = 3_000;
export type ConnectorProgress = "query_inserted" | "menu_ready" | "app_selected";
export const SELECTED_CONNECTOR_SELECTOR = '[data-id^="plugin:"][data-keyword], [app-mention-path^="app://"][contenteditable="false"]';

export interface SelectedConnector {
  composer: Locator;
  identity: string;
}

/** The configured row never appeared; no connector was selected. */
export class ConnectorMenuUnavailableError extends BrowserCompanionError {
  constructor(message:string,cause:unknown) {
    super(message,"connector_unavailable",{cause});
  }
}

export function connectorSelectionSelector(name: string): string {
  if (!name.trim() || name.length > 256 || /[\u0000-\u001f\u007f]/u.test(name)) {
    throw new Error("Connector name is invalid");
  }
  const encoded = JSON.stringify(name);
  return [
    `[data-id^="plugin:"][data-keyword=${encoded}]`,
    `[app-mention-path^="app://"][app-mention-display-name=${encoded}][contenteditable="false"]`,
  ].join(", ");
}

export function selectedConnectorControl(composer: Locator, name: string, identity?: string): Locator {
  return composer.locator(identity
    ? `[data-id=${JSON.stringify(identity)}], [app-mention-path=${JSON.stringify(identity)}][contenteditable="false"]`
    : connectorSelectionSelector(name)).filter({ visible:true });
}

export async function connectorIsSelected(composer: Locator, name: string, identity?: string): Promise<boolean> {
  const selected = selectedConnectorControl(composer, name, identity);
  const names = await selected.evaluateAll((elements, useIdentity) => elements.map(element => (
    useIdentity
      ? element.getAttribute("app-mention-path") ?? element.getAttribute("data-id")
      : element.getAttribute("data-keyword") ?? element.getAttribute("app-mention-display-name")
  )), identity !== undefined);
  const exact = names.filter(value => value === (identity ?? name)).length;
  if (exact > 1) throw new BrowserCompanionError(`ChatGPT exposed duplicate ${JSON.stringify(name)} connector selections`, "connector_unavailable");
  return exact === 1;
}

async function selectedConnectorIdentity(composer: Locator): Promise<string> {
  const identities = await composer.locator(SELECTED_CONNECTOR_SELECTOR).filter({ visible:true })
    .evaluateAll(elements => elements.map(element => element.getAttribute("app-mention-path") ?? element.getAttribute("data-id")));
  if (identities.length !== 1 || !identities[0]) {
    throw new BrowserCompanionError("ChatGPT did not expose one selected connector identity", "connector_unavailable");
  }
  return identities[0];
}

export function normalizeConnectorRowTitles(texts: readonly string[]): string[] {
  return texts
    .map(text => (text.split("\n")[0] ?? "").replace(/\s+/g, " ").trim())
    .filter(Boolean);
}

export async function clearComposerState(page: Page, connectorName: string, signal?: AbortSignal): Promise<void> {
  await page.keyboard.press("Escape").catch(() => {});
  const composer = await activeComposer(page, CONNECTOR_COMPOSER_TIMEOUT_MS, signal);
  await composer.focus({ timeout:CONNECTOR_ACTION_TIMEOUT_MS, signal });
  await composer.press(process.platform === "darwin" ? "Meta+A" : "Control+A", { timeout:CONNECTOR_ACTION_TIMEOUT_MS, signal });
  await composer.press("Backspace", { timeout:CONNECTOR_ACTION_TIMEOUT_MS, signal });
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    const settled = await activeComposer(page, CONNECTOR_COMPOSER_TIMEOUT_MS, signal);
    const text = await composerText(settled);
    const selected = await settled.locator(SELECTED_CONNECTOR_SELECTOR).filter({ visible:true }).count() > 0;
    if (!text.trim() && !selected) return;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new BrowserCompanionError("ChatGPT connector cleanup did not produce an empty composer", "external_interference");
}

async function mentionRowTitles(rows: Locator): Promise<string[]> {
  try {
    return normalizeConnectorRowTitles(await rows.filter({ visible:true }).allInnerTexts());
  } catch {
    return [];
  }
}

export async function selectConnector(
  page: Page,
  connectorName: string,
  signal?: AbortSignal,
  progress?: (stage:ConnectorProgress) => void,
): Promise<SelectedConnector> {
  let stage = "finding editor";
  const mark = (value:ConnectorProgress) => {stage=value;progress?.(value);};
  const readyComposer = async () => {
    try { return await activeComposer(page, CONNECTOR_COMPOSER_TIMEOUT_MS, signal); }
    catch(error) {
      if(signal?.aborted)throw error;
      throw new ConnectorMenuUnavailableError(`ChatGPT's connector editor was unavailable at ${stage}`,error);
    }
  };
  let composer = await readyComposer();
  if (await connectorIsSelected(composer, connectorName)) {
    if (!(await composerText(composer)).trim()) return { composer, identity:await selectedConnectorIdentity(composer) };
    await clearComposerState(page, connectorName, signal);
    composer = await readyComposer();
  }
  if ((await composerText(composer)).trim()) {
    await clearComposerState(page, connectorName, signal);
    composer = await readyComposer();
  }
  const rows = page.locator('.__menu-item[tabindex="0"], [data-mention-list-scroll-area] button[data-list-navigation-item="true"]');
  const exactRow = rows.filter({ has:page.getByText(connectorName, { exact:true }) });
  let attempts = 0;
  try {
    while (attempts < MAX_CONNECTOR_TRIGGER_ATTEMPTS) {
      attempts += 1;
      composer = await readyComposer();
      stage="inserting app query";
      // One input event avoids rerendering a long conversation for every
      // character. ProseMirror's mention search consumes the resulting input.
      await composer.fill(`@${connectorName}`, { timeout:CONNECTOR_ACTION_TIMEOUT_MS, signal });
      mark("query_inserted");
      try {
        await exactRow.waitFor({ state:"visible", timeout:CONNECTOR_MENU_TIMEOUT_MS, signal });
        mark("menu_ready");
        break;
      } catch (error) {
        if (signal?.aborted) throw error;
        const titles = await mentionRowTitles(rows);
        if (attempts >= MAX_CONNECTOR_TRIGGER_ATTEMPTS) {
          const detail = titles.length
            ? `ChatGPT connector menu exposed ${titles.map(title => JSON.stringify(title)).join(", ")}`
            : "ChatGPT connector menu did not open";
          throw new ConnectorMenuUnavailableError(
            `${detail}, but no exact row named ${JSON.stringify(connectorName)} was available`,
            error,
          );
        }
      }
    }
    if (await exactRow.count() !== 1) {
      throw new BrowserCompanionError(`ChatGPT did not expose one exact ${JSON.stringify(connectorName)} connector row`, "connector_unavailable");
    }
    // Click the verified app row directly. Enter in the composer can submit the
    // mention as a message if React closes/rebuilds the menu between checks.
    await exactRow.click({ timeout:CONNECTOR_ACTION_TIMEOUT_MS, signal });
    const selected = await readyComposer();
    // The exact menu row proves the configured display name. The selected chip can
    // expose an internal slug instead, so bind its actual app identity for Send.
    await selected.locator(SELECTED_CONNECTOR_SELECTOR).filter({ visible:true }).waitFor({ state:"visible", timeout:CONNECTOR_ACTION_TIMEOUT_MS, signal });
    const identity = await selectedConnectorIdentity(selected);
    if (!await connectorIsSelected(selected, connectorName, identity)) {
      throw new BrowserCompanionError(`ChatGPT did not retain the ${JSON.stringify(connectorName)} connector selection`, "connector_unavailable");
    }
    if ((await composerText(selected)).trim()) {
      throw new BrowserCompanionError("ChatGPT connector selection left an unexpected draft", "external_interference");
    }
    // A pointer click leaves the editor's collapsed selection where the typed
    // query was. Prompt insertion keeps a collapsed in-editor caret, so place
    // it after the retained chip; otherwise the prompt lands before the chip.
    await selected.evaluate((element:HTMLElement) => {
      element.focus();
      const owner = element.ownerDocument;
      const selection = owner.defaultView?.getSelection?.() ?? owner.getSelection?.();
      if (!selection || typeof owner.createRange !== "function") return;
      const range = owner.createRange();
      range.selectNodeContents(element);
      range.collapse(false);
      selection.removeAllRanges();
      selection.addRange(range);
    });
    mark("app_selected");
    return { composer:selected, identity };
  } catch (error) {
    if(signal?.aborted)throw error;
    try {
      await clearComposerState(page, connectorName, signal);
    } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "ChatGPT connector selection failed and its draft could not be cleared");
    }
    if(error instanceof Error&&error.name==="TimeoutError")throw new ConnectorMenuUnavailableError(`ChatGPT did not finish selecting the requested connector at ${stage}`,error);
    throw error;
  }
}

export async function verifyConnector(page: Page, connectorName: string, signal?: AbortSignal): Promise<void> {
  await selectConnector(page, connectorName, signal);
  await clearComposerState(page, connectorName, signal);
}
