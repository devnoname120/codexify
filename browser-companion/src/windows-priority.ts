import {realpath,stat} from "node:fs/promises";
import {isAbsolute,relative} from "node:path";
import {constants,getPriority,setPriority} from "node:os";

interface PrioritySession {
  send(method:"SystemInfo.getProcessInfo"):Promise<unknown>;
  detach():Promise<void>;
}
interface PriorityContext {
  browser():{newBrowserCDPSession():Promise<PrioritySession>}|null;
  once(event:"close",listener:()=>void):unknown;
  off(event:"close",listener:()=>void):unknown;
}
interface PriorityInterval {unref():unknown}
export interface WindowsPriorityDependencies {
  platform?:NodeJS.Platform;
  getPriority?:(pid:number)=>number;
  setPriority?:(pid:number,priority:number)=>void;
  schedule?:(tick:()=>void,ms:number)=>PriorityInterval;
  cancel?:(timer:PriorityInterval)=>void;
  readTimeoutMs?:number;
}

async function bounded<T>(pending:Promise<T>,timeoutMs:number):Promise<T> {
  let timer:ReturnType<typeof setTimeout>|undefined;
  try {return await Promise.race([pending,new Promise<never>((_,reject)=>{timer=setTimeout(()=>reject(new Error("Private Chrome process observation timed out")),timeoutMs);})]);}
  finally {if(timer)clearTimeout(timer);}
}

// Only a packaged, private Chrome executable may opt in. CDP supplies this
// browser instance's own processes; this never enumerates the machine's PIDs.
export async function normalizeWindowsRendererPriorities(context:PriorityContext,options:{executablePath:string;managedRuntimeRoot?:string},dependencies:WindowsPriorityDependencies={}):Promise<()=>void> {
  if((dependencies.platform??process.platform)!=="win32"||!options.managedRuntimeRoot)return ()=>{};
  let stopped=false,busy=false,session:PrioritySession|undefined,interval:PriorityInterval|undefined;
  const detached=new WeakSet<PrioritySession>();
  const detach=(target:PrioritySession)=>{if(detached.has(target))return;detached.add(target);void Promise.resolve().then(()=>target.detach()).catch(()=>{});};
  const cancel=dependencies.cancel??(timer=>clearInterval(timer as ReturnType<typeof setInterval>));
  const stop=()=>{
    if(stopped)return;stopped=true;
    if(interval)cancel(interval);
    context.off("close",stop);
    if(session)detach(session);
  };
  context.once("close",stop);
  try {
    const [root,executable]=await Promise.all([realpath(options.managedRuntimeRoot),realpath(options.executablePath)]);
    const path=relative(root,executable);
    if(!path||path===".."||path.startsWith("..\\")||path.startsWith("../")||isAbsolute(path)
      ||!(await stat(root)).isDirectory()||!(await stat(executable)).isFile()||stopped){stop();return stop;}
    const browser=context.browser();if(!browser){stop();return stop;}
    const connecting=browser.newBrowserCDPSession();
    try {session=await bounded(connecting,dependencies.readTimeoutMs??1_000);}
    catch {void connecting.then(detach,()=>{});stop();return stop;}
    if(stopped){detach(session);return stop;}
    const readPriority=dependencies.getPriority??getPriority,writePriority=dependencies.setPriority??setPriority;
    const inspect=async()=>{
      if(stopped||busy||!session)return;busy=true;
      let settled=false;
      const pending=Promise.resolve().then(()=>stopped?undefined:session!.send("SystemInfo.getProcessInfo")).finally(()=>{settled=true;});
      try {
        const result=await bounded(pending,dependencies.readTimeoutMs??1_000);
        if(stopped||!result||typeof result!=="object"||!("processInfo" in result)||!Array.isArray(result.processInfo))return;
        const seen=new Set<number>();
        for(const info of result.processInfo){
          if(stopped)return;
          if(!info||info.type!=="renderer"||!Number.isSafeInteger(info.id)||info.id<=0||info.id>0xffff_ffff||seen.has(info.id))continue;
          seen.add(info.id);
          try {
            const priority=readPriority(info.id);
            // Windows maps Idle/BelowNormal to positive values and Normal to0.
            // Preserve every already-normal or higher-priority process.
            if(!stopped&&Number.isFinite(priority)&&priority>constants.priority.PRIORITY_NORMAL)writePriority(info.id,constants.priority.PRIORITY_NORMAL);
          } catch {/* A renderer may exit between its CDP report and OS read. */}
        }
      } catch {/* Priority maintenance cannot make a usable browser unavailable. */}
      finally {
        if(settled)busy=false;
        else void pending.then(()=>{busy=false;},()=>{busy=false;});
      }
    };
    await inspect();
    if(!stopped){interval=(dependencies.schedule??((tick,ms)=>setInterval(tick,ms)))(()=>{void inspect();},2_000);interval.unref();}
  } catch {stop();}
  return stop;
}
