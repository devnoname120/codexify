import type {Page} from 'patchright';
import {observeReadOnly} from './observation';

const rendererMemoryChecks=new WeakMap<Page,number>();

/** Run while the action still owns its page. Reclaim unreachable old document
 * heaps without deleting browser caches, cookies, drafts or conversation data. */
export async function maintainRendererMemory(page:Page,now=Date.now()):Promise<void> {
  const previous=rendererMemoryChecks.get(page);
  if(page.isClosed() || previous!==undefined && now-previous<60_000)return;
  rendererMemoryChecks.set(page,now);
  let session:Awaited<ReturnType<ReturnType<Page['context']>['newCDPSession']>>|undefined;
  try {
    const connecting=page.context().newCDPSession(page);
    try {session=await observeReadOnly(connecting,undefined,1_000);}
    catch {void connecting.then(late=>late.detach(),()=>{}).catch(()=>{});return;}
    const before=await observeReadOnly(session.send('Runtime.getHeapUsage'),undefined,1_000);
    if(!Number.isFinite(before.usedSize)||before.usedSize<768*1024*1024)return;
    await observeReadOnly(session.send('HeapProfiler.collectGarbage'),undefined,2_000);
    const after=await observeReadOnly(session.send('Runtime.getHeapUsage'),undefined,1_000);
    console.error(`[browser-memory] at=${new Date().toISOString()} beforeBytes=${before.usedSize} afterBytes=${after.usedSize}`);
  } catch { /* Memory maintenance cannot turn a completed Send into a failure. */ }
  finally {if(session)await observeReadOnly(session.detach(),undefined,1_000).catch(()=>{});}
}
