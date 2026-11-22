import {lstatSync,readFileSync} from 'node:fs';
import {join} from 'node:path';
import type {SingleBrowserSurface} from './chatgpt-driver';
import type {SingleWindowCoordinator} from './browser-window';
import type {Page} from 'patchright';
import {canonicalChatGptConversationUrl} from './protocol';
import {observeReadOnly} from './observation';
import {scrollConversationToLatest} from './conversation-approval';

export type AutoApprovalPolicy={connectorName:string;accountFingerprint:string};
export function readAutoApprovalPolicy(root:string):AutoApprovalPolicy|null {
  const read=(path:string,limit:number)=>{
    const info=lstatSync(path);if(!info.isFile()||info.isSymbolicLink()||info.size>limit)throw Error('Invalid approval policy file');
    return JSON.parse(readFileSync(path,'utf8'));
  };
  try {
    const config=read(join(root,'..','codexify.config.json'),2*1024*1024),policy=config?.agentChat?.automation;
    if(config?.agentChat?.enabled!==true||policy?.enabled!==true||policy.autoApproveToolCalls!==true
      ||typeof policy.connectorName!=='string'||!policy.connectorName.trim()||policy.connectorName.length>128)return null;
    let fingerprint:unknown;
    try {fingerprint=read(join(root,'account.json'),4096).fingerprint;}
    catch(error) {
      if((error as NodeJS.ErrnoException).code!=='ENOENT')return null;
      // Restored authenticated profiles need not pass through explicit Login.
      // The server's verified catalog provides their account binding; the
      // approval operation still rechecks the live account before clicking.
      fingerprint=read(join(root,'catalog-cache.json'),2*1024*1024)?.catalog?.accountFingerprint;
    }
    if(typeof fingerprint!=='string'||!/^[a-f0-9]{64}$/.test(fingerprint))return null;
    return {connectorName:policy.connectorName,accountFingerprint:fingerprint};
  }catch{return null;}
}

/** Idle, visible-page checks only. Never navigate, submit prompts, or overlap a
 * controller action. Read the owner's setting again on every tick. */
export function startAutoApprovalMonitor(
  coordinator:Pick<SingleWindowCoordinator,'approvePendingConversation'>&{readonly busy?:boolean},
  surface:Pick<SingleBrowserSurface,'page'>&{approvalPages?:()=>Promise<Page[]>;closed?:Promise<void>},root:string,
  options:{policy?:()=>AutoApprovalPolicy|null;intervalMs?:number}={},
):()=>void {
  let stopped=false,busy=false,nextPage=0;
  const cancellation=new AbortController();
  const tick=async()=>{
    if(stopped||busy||coordinator.busy)return;busy=true;
    try {
      const policy=(options.policy??(()=>readAutoApprovalPolicy(root)))();if(!policy)return;
      const signal=AbortSignal.any([cancellation.signal,AbortSignal.timeout(8_000)]);
      const pages=await observeReadOnly(surface.approvalPages?.()??surface.page().then(page=>[page]),signal,1_000);
      for(let checked=0;checked<pages.length;checked++) {
        const page=pages[nextPage++%pages.length]!;if(page.isClosed())continue;
        if(!/^\/(?:g\/[^/]+\/)?c\/[^/]+\/?$/.test(new URL(page.url()).pathname))continue;
        const url=canonicalChatGptConversationUrl(page.url());signal.throwIfAborted();
        // Empty polls must not acquire a window lease: that invalidates passive
        // health proof and can repeatedly defer queued native work. This probe
        // only exposes the latest turn; the leased path rechecks all targets.
        if(coordinator.busy)return;
        await observeReadOnly(scrollConversationToLatest(page),signal,1_000);
        signal.throwIfAborted();if(coordinator.busy)return;
        const cards=await observeReadOnly(page.locator('[data-codex-approval-surface="true"]').filter({visible:true}).count(),signal,1_000);
        signal.throwIfAborted();if(!cards)continue;if(coordinator.busy)return;
        const result=await coordinator.approvePendingConversation({url,expectedAccountFingerprint:policy.accountFingerprint,connectorName:policy.connectorName},signal,false);
        if(result.approved)console.error(`[browser-approval] at=${new Date().toISOString()} scope=conversation approved configured connector request`);
      }
    }catch{ /* A closed/busy page or unreadable policy is retried, never submitted. */ }
    finally {busy=false;}
  };
  const timer=setInterval(()=>{void tick();},options.intervalMs??2_000);timer.unref();
  const stop=()=>{if(stopped)return;stopped=true;clearInterval(timer);cancellation.abort();};
  void surface.closed?.then(stop,stop);void tick();return stop;
}
