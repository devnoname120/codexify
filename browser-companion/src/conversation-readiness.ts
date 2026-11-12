import type {Page} from "patchright";
import {throwIfSessionAttention} from "./browser-session";
import {activeComposer} from "./composer";
import {BrowserCompanionError} from "./errors";
import {throwIfConversationRateLimited} from "./navigation";

/** DOMContentLoaded precedes ChatGPT's editor hydration, especially in long chats. */
export async function waitForConversationComposer(page:Page,signal?:AbortSignal,timeoutMs=20_000):Promise<void> {
  const deadline=Date.now()+timeoutMs;
  let lastError:unknown;
  do {
    signal?.throwIfAborted();
    await throwIfSessionAttention(page);
    // A real sign-in surface should still fail promptly. Ignore sign-in links
    // inside conversation content, where an answer may quote any URL.
    if(new URL(page.url()).pathname.startsWith("/auth/") || await page.locator(
      'a[href="/auth/login"]:not(article a, [data-turn-key] a, [data-turn-id] a, [data-message-author-role] a)',
    ).filter({visible:true}).count()) {
      throw new BrowserCompanionError("Sign in to ChatGPT in the browser companion","authentication_required");
    }
    try {
      await activeComposer(page,Math.max(1,Math.min(250,deadline-Date.now())),signal);
      signal?.throwIfAborted();
      await throwIfSessionAttention(page);
      return;
    } catch(error) {
      signal?.throwIfAborted();
      if(error instanceof BrowserCompanionError)throw error;
      throwIfConversationRateLimited(page,true);
      lastError=error;
    }
  } while(Date.now()<deadline);
  // Missing/disabled editors are not evidence of an expired account. The
  // driver separately verifies fresh account identity before it can send.
  throw new BrowserCompanionError("ChatGPT's conversation editor did not finish loading; nothing was sent","editor_unavailable",{cause:lastError});
}
