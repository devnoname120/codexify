import {lstatSync,readFileSync} from 'node:fs';
import {join} from 'node:path';
import type {SingleBrowserSurface} from './chatgpt-driver';
import type {SingleWindowCoordinator} from './browser-window';
import {canonicalChatGptConversationUrl} from './protocol';
import {observeReadOnly} from './observation';

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
    const account=read(join(root,'account.json'),4096);
    if(typeof account.fingerprint!=='string'||!/^[a-f0-9]{64}$/.test(account.fingerprint))return null;
    return {connectorName:policy.connectorName,accountFingerprint:account.fingerprint};
  }catch{return null;}
}

/** Idle, visible-page checks only. Never navigate, submit prompts, or overlap a
 * controller action. Read the owner's setting again on every tick. */
export function startAutoApprovalMonitor(
  coordinator:Pick<SingleWindowCoordinator,'approvePendingConversation'>&{readonly busy?:boolean},
  surface:Pick<SingleBrowserSurface,'page'>&{closed?:Promise<void>},root:string,
  options:{policy?:()=>AutoApprovalPolicy|null;intervalMs?:number}={},
):()=>void {
  let stopped=false,busy=false;
  const cancellation=new AbortController();
  const tick=async()=>{
    if(stopped||busy||coordinator.busy)return;busy=true;
    try {
      const policy=(options.policy??(()=>readAutoApprovalPolicy(root)))();if(!policy)return;
      const signal=AbortSignal.any([cancellation.signal,AbortSignal.timeout(4_000)]);
      const page=await observeReadOnly(surface.page(),signal,1_000);if(page.isClosed())return;
      const url=canonicalChatGptConversationUrl(page.url());signal.throwIfAborted();
      // Empty polls must not acquire a window lease: that invalidates passive
      // health proof and can repeatedly defer queued native work. This probe
      // is read-only; the leased approval path rechecks all targets before use.
      if(coordinator.busy)return;
      const cards=await observeReadOnly(page.locator('[data-codex-approval-surface="true"]').filter({visible:true}).count(),signal,1_000);
      signal.throwIfAborted();if(!cards||coordinator.busy)return;
      const result=await coordinator.approvePendingConversation({url,expectedAccountFingerprint:policy.accountFingerprint,connectorName:policy.connectorName},signal,false,true);
      if(result.approved)console.error(`[browser-approval] at=${new Date().toISOString()} approved configured connector request`);
    }catch{ /* A closed/busy page or unreadable policy is retried, never submitted. */ }
    finally {busy=false;}
  };
  const timer=setInterval(()=>{void tick();},options.intervalMs??2_000);timer.unref();
  const stop=()=>{if(stopped)return;stopped=true;clearInterval(timer);cancellation.abort();};
  void surface.closed?.then(stop,stop);void tick();return stop;
}
