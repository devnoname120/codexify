// Adapted from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/browser-worker.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import type { Locator, Page } from "patchright";
import { BrowserCompanionError, CONVERSATION_UNAVAILABLE_MESSAGE } from "./errors";
import {uiLabelPattern,uiTemplatePattern} from "./ui-localization";
import {recordConversationUnavailable,throwIfConversationRateLimited} from "./navigation";
import {composerText,isAutomationGoalDraft} from "./composer";
import {CHATGPT_COMPOSER_SELECTOR} from "./session";
import {observeReadOnly} from "./observation";

const rateLimitDialog = (page: Page): Locator => page.locator('[role="dialog"]')
  .filter({ hasText:uiLabelPattern("rateTitle",false) })
  .filter({ hasText:uiLabelPattern("rateBody",false) })
  .last();

const expiredSessionAlert = (page: Page): Locator => page
  .locator('[role="alert"], [role="dialog"]')
  .filter({ hasText:uiLabelPattern("expiredTitle",false) })
  .last();

const subscriptionFailureAlert = (page: Page): Locator => page
  .locator('[role="alert"]')
  .filter({ hasText:uiTemplatePattern("subscriptionFailure") })
  .last();

// Error semantics and the support destination are stable across UI languages.
// Scope the fallback to alerts/error surfaces so ordinary answer links do not match.
const terminalErrorAlert = (page: Page): Locator => page
  .locator('[role="alert"], [data-testid="conversation-turn-error"], [data-testid="conversation-error"], [data-error-code]')
  .filter({hasText:uiLabelPattern("terminalFailure",false)})
  .filter({hasText:/help\.openai\.com/i}).filter({visible:true}).last();

export async function conversationUnavailableVisible(page: Page): Promise<boolean> {
  const url = new URL(page.url());
  if (url.origin !== "https://chatgpt.com" || !/^\/(?:g\/[^/]+\/)?c\/[^/]+\/?$/.test(url.pathname)) return false;
  const unavailable = await page.evaluate(() => {
    const visible = (element: Element): boolean => {
      if (!element.getClientRects().length || element.closest('[hidden], [inert], [aria-hidden="true"]')) return false;
      for (let node: Element | null = element; node; node = node.parentElement) {
        const style = getComputedStyle(node);
        if (style.display === "none" || style.visibility === "hidden" || style.opacity === "0") return false;
      }
      return true;
    };
    const conversationContent = 'form, [role="textbox"], textarea, [contenteditable="true"], article, [data-turn-key], [data-turn-id], [data-message-author-role]';
    // A full conversation can retain its transcript and editor. Recognize the
    // app's terminal banner, not a quoted sentence in an answer or user message.
    const protectedContent='pre,code,blockquote,.markdown,[data-user-message-bubble],[data-message-author-role="user"],[data-conversation-role="user"]';
    const approvals='[role="dialog"],[role="alertdialog"],[data-testid="tool-approval-card"],[data-codex-approval-surface="true"]';
    if(![...document.querySelectorAll(approvals)].some(visible)) {
      const maximum=/You['’]ve reached the maximum length for this conversation, but you can keep talking by starting a new chat\./i;
      const red=(color:string)=>{const parts=color.match(/[\d.]+/g)?.map(Number);return !!parts&&parts[0]!>=parts[1]!+40&&parts[0]!>=parts[2]!+40;};
      for(const button of document.querySelectorAll('button,[role="button"],a[href]')) {
        if(button.closest(protectedContent)||!/^start new chat$/i.test((button.textContent??'').replace(/\s+/g,' ').trim())||!visible(button))continue;
        for(let panel=button.parentElement,depth=0;panel&&depth<5;panel=panel.parentElement,depth++) {
          if(panel.closest(protectedContent))break;
          const text=(panel.textContent??'').replace(/\s+/g,' ').trim();
          if(text.length>600)break;
          if(!maximum.test(text)||!visible(panel))continue;
          const style=getComputedStyle(panel);
          if(panel.matches('[role="alert"],[data-error-code]')
            ||parseFloat(style.borderTopWidth)>0&&red(style.borderTopColor))return 'limit';
        }
      }
    }
    // The inline load failure replaces the transcript, with one message and
    // one retry button. Match its app-owned structure, never translated text.
    for (const target of document.querySelectorAll('[data-chatgpt-conversation-selection-target][data-thread-find-target="conversation"]')) {
      if (!target.closest('[data-mcp-app-portal-target="true"][data-thread-user-message-navigation-content="true"]')
        || !target.closest('[data-request-input-activity-root="true"]')
        || target.closest('article, [data-turn-key], [data-turn-id], [data-message-author-role]')
        || target.querySelector(conversationContent) || !visible(target)) continue;
      const children = [...target.children].filter(visible);
      if (children.length !== 1) continue;
      const panel = children[0]!;
      const button = panel.firstElementChild;
      if (panel.matches('.rounded-xl.border.border-default.p-4.text-sm.text-default')
        && panel.children.length === 1 && button?.matches('button[type="button"]') && visible(button)
        && [...panel.childNodes].some(node => node.nodeType === Node.TEXT_NODE && !!node.textContent?.trim())) return 'unavailable';
    }
    // The original full-page layout also has no composer or conversation turns.
    const main = document.querySelector('[data-app-shell-focus-area="main"]');
    if (!main || main.querySelector(conversationContent)) return false;
    const panels = main.querySelectorAll(':scope > div > .flex.flex-col.items-center.justify-center > .flex.flex-col.items-center.justify-center.text-center.max-w-xl');
    return [...panels].some(panel => {
      if (panel.children.length !== 2 || !visible(panel)) return false;
      const [message, actions] = [...panel.children];
      const heading = message!.querySelector(':scope > .font-medium.text-default.text-lg');
      const buttons = actions!.querySelectorAll('button');
      return message!.matches('.flex.flex-col.items-center')
        && actions!.matches('.flex.flex-wrap.items-center.justify-center')
        && !!heading?.textContent?.trim() && visible(heading)
        && buttons.length === 1 && buttons[0]!.matches('button[type="button"]') && visible(buttons[0]!)
        && !panel.querySelector('a, input, select, [role="button"]');
    }) ? 'unavailable' : false;
  });
  if(unavailable==='limit') {
    const composers=page.locator(CHATGPT_COMPOSER_SELECTOR).filter({visible:true});
    const count=await composers.count();
    if(count>1)return false;
    if(count===1) {
      const draft=await composerText(composers.first());
      if(draft&&!isAutomationGoalDraft(draft))return false;
    }
  }
  return Boolean(unavailable);
}

export async function toolApprovalVisible(page: Page): Promise<boolean> {
  // This guard protects pairing-draft replacement. Any visible modal blocks it;
  // recognizing an English approval question is neither necessary nor sufficient.
  return page.locator('[role="dialog"], [role="alertdialog"], [data-testid="tool-approval-card"], [data-codex-approval-surface="true"]')
    .filter({visible:true}).count().then(count=>count>0).catch(()=>false);
}

export async function throwIfSessionAttention(page: Page,signal?:AbortSignal): Promise<void> {
  const read=<T>(operation:()=>Promise<T>):Promise<T>=>{
    signal?.throwIfAborted();return observeReadOnly(operation(),signal);
  };
  throwIfConversationRateLimited(page);
  if (await read(()=>expiredSessionAlert(page).isVisible().catch(() => false))) {
    throw new BrowserCompanionError("The ChatGPT session has expired. Sign in again in the browser companion.", "authentication_required");
  }
  if (await read(()=>rateLimitDialog(page).isVisible().catch(() => false))) {
    throw new BrowserCompanionError("ChatGPT rate limit: too many requests. Retry only after the account cooldown clears.", "rate_limited");
  }
  if (await read(()=>subscriptionFailureAlert(page).isVisible().catch(() => false))) {
    throw new BrowserCompanionError("ChatGPT could not load the current subscription.", "failed");
  }
  if (await read(()=>conversationUnavailableVisible(page))) {
    signal?.throwIfAborted();
    throwIfConversationRateLimited(page,true);
    recordConversationUnavailable(page);
    throw new BrowserCompanionError(CONVERSATION_UNAVAILABLE_MESSAGE, "conversation_unavailable");
  }
  if (await read(()=>page.getByTestId("regenerate-thread-error-button").last().isVisible().catch(() => false))
    || await read(()=>terminalErrorAlert(page).isVisible().catch(() => false))) {
    throw new BrowserCompanionError("ChatGPT displayed a terminal response error.", "failed");
  }
}
