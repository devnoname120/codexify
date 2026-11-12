import type { Page } from "patchright";
import { readChatGptAccount } from "./account";
import { BrowserCompanionError } from "./errors";
import { canonicalChatGptConversationUrl, type ConversationApprovalRequest } from "./protocol";

const APPROVAL_SELECTOR = '[data-codex-approval-surface="true"], [data-testid="tool-approval-card"]';

/** Old unanswered cards must not mask the blocker in the current turn. */
export async function latestConversationApprovalCards(page:Page) {
  const groups = page.locator('[data-turn-key]').filter({visible:true});
  if (await groups.count()) return groups.last().locator(APPROVAL_SELECTOR).filter({visible:true});
  const legacy = page.locator('article[data-testid^="conversation-turn-"], [data-turn-id-container]').filter({visible:true});
  if (await legacy.count()) return legacy.last().locator(APPROVAL_SELECTOR).filter({visible:true});
  return page.locator(APPROVAL_SELECTOR).filter({visible:true});
}

export async function throwIfConversationApproval(page:Page):Promise<void> {
  if (await (await latestConversationApprovalCards(page)).count()) {
    throw new BrowserCompanionError("ChatGPT is waiting for a connector approval before this conversation can continue", "approval_required");
  }
}

/** Only the connector card in the verified conversation may grant conversation scope. */
export async function approveConversationCard(page:Page, request:ConversationApprovalRequest, expectedAppId:string|undefined, signal:AbortSignal,mode:'conversation'|'once'='conversation'):Promise<boolean> {
  const sameConversation = () => {
    try { return canonicalChatGptConversationUrl(page.url()) === canonicalChatGptConversationUrl(request.url); }
    catch { return false; }
  };
  if (!sameConversation()) return false;
  if(mode==='once'&&!expectedAppId)return false;
  if(!await page.locator(APPROVAL_SELECTOR).filter({visible:true}).count())return false;
  const cards = (await latestConversationApprovalCards(page)).and(page.locator('[data-codex-approval-surface="true"]'));
  if (await cards.count() !== 1) return false;
  const card = cards.first();
  const trusted = () => card.evaluate((element,{name,appId}) => {
    const alert = element.querySelector('[role="alert"]');
    const heading = alert?.querySelector('.text-secondary > span.wrap-anywhere');
    const form = element.querySelector('form');
    const buttons = form?.querySelectorAll('button');
    const trigger = form?.querySelector('button[type="button"][aria-haspopup="menu"]');
    if (element.querySelectorAll('[role="alert"]').length !== 1 || element.querySelectorAll('form').length !== 1
      || heading?.textContent?.trim() !== name || !form || buttons?.length !== 3 || !trigger
      // Verify the separate denial and split approval controls by structure;
      // ChatGPT changes their color classes independently of their behavior.
      || buttons[0]?.getAttribute('type') !== 'button' || buttons[1]?.getAttribute('type') !== 'button'
      || buttons[0]?.hasAttribute('aria-haspopup') || buttons[1]?.hasAttribute('aria-haspopup')
      || buttons[2] !== trigger || trigger.previousElementSibling !== buttons[1]
      || trigger.parentElement?.querySelectorAll('button').length !== 2
      || trigger.parentElement?.parentElement !== buttons[0]?.parentElement) return false;
    // Match the connector chip in this same turn, not names mentioned by an answer.
    const turn = element.closest('[data-turn-key]');
    const mentions = [...(turn?.querySelectorAll('[data-user-message-bubble] [data-prompt-link-href^="app://"]') ?? [])]
      .filter(mention => mention.textContent?.trim() === name)
      .map(mention => mention.getAttribute('data-prompt-link-href')!.slice(6));
    if(mentions.length)return mentions.length === 1 && /^asdk_app_[A-Za-z0-9_]+$/.test(mentions[0]!) && (!appId || mentions[0] === appId);
    // A later user turn can inherit a connector selected earlier in this same
    // conversation. A platform-owned card for the configured, account-bound
    // connector can omit its chip. Any explicit conflicting user identity fails.
    if(!appId||!turn)return false;
    const inherited=new Set([...document.querySelectorAll('[data-user-message-bubble] [data-prompt-link-href^="app://"]')]
      .filter(mention=>mention.textContent?.trim()===name)
      .map(mention=>mention.getAttribute('data-prompt-link-href')!.slice(6)));
    return inherited.size===0||inherited.size===1&&inherited.has(appId);
  }, {name:request.connectorName,appId:expectedAppId});
  if (!await trusted()) return false;
  const trigger = card.locator('form button[type="button"][aria-haspopup="menu"]');
  const menus = page.locator('[role="menu"]').filter({visible:true});
  const existingMenus = await menus.count();
  if (existingMenus > 1 || !await trigger.isEnabled()) return false;
  if (existingMenus) {
    // A previous attempt or the user may already have opened this card's
    // options. Only reuse an explicitly associated approval menu.
    const menu = menus.first(), id = await trigger.getAttribute('id');
    const controls = await trigger.getAttribute('aria-controls'), labelledBy = await menu.getAttribute('aria-labelledby');
    if (await trigger.getAttribute('aria-expanded') !== 'true'
      || !(controls && controls === await menu.getAttribute('id')
        || id && labelledBy?.split(/\s+/).includes(id))) return false;
  }
  const verify = async () => {
    signal.throwIfAborted();
    if (!sameConversation() || (await readChatGptAccount(page)).fingerprint !== request.expectedAccountFingerprint || !await trusted()) {
      throw new BrowserCompanionError("ChatGPT changed account or conversation before approving the connector", "external_interference");
    }
  };
  await verify();
  if(mode==='once'){
    const once=card.locator('form button[type="button"].bg-primary-solid:not([aria-haspopup])');
    if(await once.count()!==1||!await once.isEnabled())return false;
    await verify();await once.click({timeout:750,signal});
    await card.waitFor({state:'hidden',timeout:1_500,signal});return true;
  }
  let opened = false;
  try {
    if (!existingMenus) await trigger.click({timeout:750,signal});
    opened = true;
    await menus.first().waitFor({state:"visible",timeout:750,signal});
    if (await menus.count() !== 1 || await trigger.getAttribute('aria-expanded') !== 'true') return false;
    const menu = menus.first(), triggerId = await trigger.getAttribute('id');
    const controls = await trigger.getAttribute('aria-controls'), labelledBy = await menu.getAttribute('aria-labelledby');
    if (controls && await menu.getAttribute('id') !== controls || labelledBy && !labelledBy.split(/\s+/).includes(triggerId ?? '')) return false;
    // The supplied menu has exactly two choices: once, then the named connector
    // for this conversation. Verify that shape and the configured name, in any language.
    const items = menu.locator('[role="menuitem"], [role="menuitemradio"]').filter({visible:true});
    if (await items.count() !== 2) return false;
    const labels = await items.allTextContents();
    if (labels[0]!.includes(request.connectorName) || !labels[1]!.includes(request.connectorName)) return false;
    const choice = items.nth(1);
    if (!await choice.isEnabled() || await choice.getAttribute('aria-disabled') === 'true') return false;
    await verify();
    if (!await card.isVisible() || !await menu.isVisible()) return false;
    const currentLabels = await items.allTextContents();
    if (currentLabels.length !== 2 || currentLabels[0]!.includes(request.connectorName) || !currentLabels[1]!.includes(request.connectorName)) return false;
    await choice.click({timeout:750,signal});
    await card.waitFor({state:"hidden",timeout:1_500,signal});
    return true;
  } finally {
    if (opened && sameConversation() && await trigger.isVisible().catch(()=>false)
      && await trigger.getAttribute('aria-expanded').catch(()=>null) === 'true') {
      await page.keyboard.press('Escape').catch(()=>{});
    }
  }
}
