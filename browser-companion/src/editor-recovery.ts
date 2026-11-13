import type {Page} from "patchright";
import {readChatGptAccount} from "./account";
import {throwIfSessionAttention} from "./browser-session";
import {throwIfConversationApproval} from "./conversation-approval";
import {activeComposer,ensureEmptyOwnedComposer} from "./composer";
import {waitForConversationComposer} from "./conversation-readiness";
import {BrowserCompanionError} from "./errors";
import {navigatePage,withConversationCooldownBypass} from "./navigation";
import {canonicalChatGptConversationUrl} from "./protocol";
import {stopControls} from "./stop-controls";
import {assertNewChatDraft} from "./new-chat";

/** Recover a stuck editor only before insertion/Send, never discard a user draft. */
export async function refreshConversationEditor(page:Page,expectedUrl:string,account:string,signal:AbortSignal,
  progress:(step:"started"|"document_loaded"|"ready")=>void,draft?:{projectUrl:string|null}):Promise<void> {
  const assertTarget=async()=>{
    signal.throwIfAborted();
    await throwIfSessionAttention(page);
    await throwIfConversationApproval(page);
    if(draft)await assertNewChatDraft(page,draft.projectUrl);
    else if(canonicalChatGptConversationUrl(page.url())!==canonicalChatGptConversationUrl(expectedUrl))
      throw new BrowserCompanionError("ChatGPT changed conversations before editor recovery","external_interference");
    if((await readChatGptAccount(page)).fingerprint!==account)
      throw new BrowserCompanionError("ChatGPT account changed before editor recovery","authentication_required");
    await ensureEmptyOwnedComposer(await activeComposer(page,1_000,signal));
    if(await stopControls(page).count())
      throw new BrowserCompanionError("ChatGPT's prior response is still running; editor refresh deferred before Send","editor_unavailable");
  };
  await assertTarget();
  progress("started");
  // The caller permits this once per action after a confirmed missing control.
  // Keep reserving the shared navigation budget for later automatic loads.
  const reloadUrl=draft ? draft.projectUrl||"https://chatgpt.com/" : expectedUrl;
  await withConversationCooldownBypass(true,()=>navigatePage(page,reloadUrl,{reload:page.url()===reloadUrl,signal,timeout:10_000}));
  progress("document_loaded");
  await waitForConversationComposer(page,signal,15_000);
  await assertTarget();
  progress("ready");
}
