import {BrowserCompanionError} from "./errors";

// Only for read-only observations. An abandoned read may settle later, but must
// never be followed by a UI mutation without another cancellation check.
export async function observeReadOnly<T>(pending:Promise<T>, signal?:AbortSignal, timeoutMs=5_000):Promise<T> {
  if (signal?.aborted) {
    // The read has already started when passed as a Promise. Observe its late
    // rejection even when cancellation happened before this helper was called.
    void pending.catch(() => {});
    signal.throwIfAborted();
  }
  let timer:ReturnType<typeof setTimeout>|undefined;
  let aborted:(()=>void)|undefined;
  try {
    return await Promise.race([pending,new Promise<never>((_,reject)=>{
      aborted=()=>reject(signal?.reason??new DOMException("Observation cancelled","AbortError"));
      signal?.addEventListener("abort",aborted,{once:true});
      timer=setTimeout(()=>reject(new BrowserCompanionError("Browser observation did not settle; the current browser and draft were preserved","editor_unavailable")),timeoutMs);
    })]);
  } finally {if(timer)clearTimeout(timer);if(aborted)signal?.removeEventListener("abort",aborted);}
}

export class RendererProbeGate {
  private failures=0;
  private since=0;
  record(responsive:boolean,now=Date.now()):boolean {
    if(responsive){this.failures=0;this.since=0;return true;}
    if(!this.failures)this.since=now;
    this.failures++;
    // A slow renderer during load or a busy machine is not a crash. Require
    // repeated real probes over a full minute before declaring it unavailable.
    return this.failures<3 || now-this.since<60_000;
  }
}
