import {afterEach,expect,test} from "bun:test";
import {EventEmitter} from "node:events";
import {mkdtemp,mkdir,writeFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {normalizeWindowsRendererPriorities,type WindowsPriorityDependencies} from "../src/windows-priority";

const cleanups:Array<()=>Promise<void>>=[];
afterEach(async()=>{for(const cleanup of cleanups.splice(0).reverse())await cleanup();});
const settle=()=>new Promise(resolve=>setTimeout(resolve,0));
function deferred<T>(){let resolve!:(value:T)=>void;const promise=new Promise<T>(yes=>{resolve=yes;});return {promise,resolve};}
async function fixture(overrides:WindowsPriorityDependencies={},read?:()=>Promise<unknown>){
  const root=await mkdtemp(join(tmpdir(),"codexify-priority-")),runtime=join(root,"runtime"),executable=join(runtime,"chrome.exe");
  await mkdir(runtime);await writeFile(executable,"private fixture");
  let reads=0,connections=0,detaches=0,cancelled=0,unrefed=0,tick:(()=>void)|undefined;
  const queried:number[]=[],writes:Array<[number,number]>=[];
  const context=Object.assign(new EventEmitter(),{browser:()=>({newBrowserCDPSession:async()=>{connections++;return {
    send:async(method:"SystemInfo.getProcessInfo")=>{expect(method).toBe("SystemInfo.getProcessInfo");reads++;return read?read():{processInfo:[{type:"renderer",id:10}]};},
    detach:async()=>{detaches++;},
  };}})});
  const dependencies:WindowsPriorityDependencies={platform:"win32",getPriority:pid=>{queried.push(pid);return 19;},setPriority:(pid,priority)=>{writes.push([pid,priority]);},schedule:(callback,ms)=>{expect(ms).toBe(2_000);tick=callback;return {unref:()=>{unrefed++;}};},cancel:()=>{cancelled++;},readTimeoutMs:20,...overrides};
  let stop:(()=>void)|undefined;
  const start=async(options:{executablePath:string;managedRuntimeRoot?:string}={executablePath:executable,managedRuntimeRoot:runtime})=>{stop=await normalizeWindowsRendererPriorities(context,options,dependencies);return stop;};
  cleanups.push(async()=>{stop?.();context.emit("close");await settle();await rm(root,{recursive:true,force:true});});
  return {root,runtime,executable,context,dependencies,start,queried,writes,tick:()=>tick?.(),stats:()=>({reads,connections,detaches,cancelled,unrefed})};
}

test("only private CDP renderer PIDs below Normal are raised to Normal",async()=>{
  const queried:number[]=[],writes:Array<[number,number]>=[];
  const values=new Map([[10,19],[11,10],[12,0],[13,-7],[14,-14],[17,19],[18,1]]);
  const f=await fixture({getPriority:pid=>{queried.push(pid);if(pid===16)throw Error("exited");return values.get(pid)!;},setPriority:(pid,priority)=>{writes.push([pid,priority]);if(pid===17)throw Error("exited");}},async()=>({processInfo:[
    {type:"browser",id:1},{type:"GPU",id:2},{type:"utility",id:3},
    ...[10,11,12,13,14,16,17,18,10,0,-1,1.5,Number.MAX_SAFE_INTEGER,"19",NaN].map(id=>({type:"renderer",id})),
  ]}));
  await f.start();expect(queried).toEqual([10,11,12,13,14,16,17,18]);
  expect(writes).toEqual([[10,0],[11,0],[17,0],[18,0]]);expect(f.stats().unrefed).toBe(1);
});

test("priority maintenance requires Windows and a canonical executable inside the packaged runtime",async()=>{
  const other=await fixture({platform:"linux"});await other.start();expect(other.stats().connections).toBe(0);
  const missing=await fixture();await missing.start({executablePath:missing.executable,managedRuntimeRoot:undefined});expect(missing.stats().connections).toBe(0);
  const outside=await fixture();const external=join(outside.root,"chrome.exe");await writeFile(external,"external");
  await outside.start({executablePath:external,managedRuntimeRoot:outside.runtime});expect(outside.stats().connections).toBe(0);expect(outside.writes).toEqual([]);
});

test("an unresolved bounded CDP read never overlaps another read or writes from its late result",async()=>{
  const pending=deferred<unknown>();let calls=0;
  const f=await fixture({},()=>++calls===1?pending.promise:Promise.resolve({processInfo:[{type:"renderer",id:10}]}));
  await f.start();expect(f.stats().reads).toBe(1);
  f.tick();f.tick();await settle();expect(f.stats().reads).toBe(1);
  pending.resolve({processInfo:[{type:"renderer",id:10}]});await settle();expect(f.writes).toEqual([]);
  f.tick();await settle();expect(f.stats().reads).toBe(2);expect(f.writes).toEqual([[10,0]]);
});

test("context close cancels polling and detaches once without late OS input",async()=>{
  const pending=deferred<unknown>();let calls=0;
  const f=await fixture({},()=>++calls===1?Promise.resolve({processInfo:[]}):pending.promise);
  const stop=await f.start();f.tick();await settle();expect(f.stats().reads).toBe(2);
  f.context.emit("close");stop();await settle();
  pending.resolve({processInfo:[{type:"renderer",id:10}]});await settle();f.tick();await settle();
  expect(f.writes).toEqual([]);expect(f.stats()).toMatchObject({reads:2,detaches:1,cancelled:1});
});

test("a close during an OS priority observation prevents the subsequent write",async()=>{
  let close=()=>{};const f=await fixture({getPriority:()=>{close();return 19;}});close=()=>{f.context.emit("close");};
  await f.start();await settle();expect(f.writes).toEqual([]);expect(f.stats().detaches).toBe(1);expect(f.stats().unrefed).toBe(0);
});

test("CDP errors remain nonfatal and a later poll can normalize a new renderer",async()=>{
  let calls=0;const f=await fixture({},async()=>{if(++calls===1)throw Error("transport unavailable");return {processInfo:[{type:"renderer",id:22}]};});
  await f.start();expect(f.writes).toEqual([]);f.tick();await settle();expect(f.writes).toEqual([[22,0]]);
});

test("failed or delayed browser session creation cannot block startup or leave a live session",async()=>{
  const failed=await fixture();failed.context.browser=()=>({newBrowserCDPSession:async()=>{throw Error("closed");}});
  await failed.start();expect(failed.writes).toEqual([]);expect(failed.stats().unrefed).toBe(0);
  const late=await fixture();type Session=Awaited<ReturnType<ReturnType<typeof late.context.browser>["newBrowserCDPSession"]>>;
  const pending=deferred<Session>();let detached=0;
  late.context.browser=()=>({newBrowserCDPSession:()=>pending.promise});
  await late.start();expect(late.stats().unrefed).toBe(0);
  pending.resolve({send:async()=>({processInfo:[{type:"renderer",id:10}]}),detach:async()=>{detached++;}});
  await settle();expect(detached).toBe(1);expect(late.writes).toEqual([]);
});
