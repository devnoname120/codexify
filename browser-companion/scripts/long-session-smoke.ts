import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,sep} from 'node:path';
import {chromium} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {backgroundChromeArgs} from '../src/background-chrome';
import {maintainRendererMemory} from '../src/renderer-memory';

// A disposable local profile; the user's chats and managed browser are untouched.
const markup='<!doctype html><title>long session fixture</title><script>window.retained=Array.from({length:250000},(_,i)=>i)</script>'+Array.from({length:500},(_,i)=>`<article data-turn-id="${i}"><p>Local fixture turn ${i}</p></article>`).join('');
const server=createServer((_request,response)=>{response.setHeader('Content-Type','text/html');response.end(markup);});
await new Promise<void>(yes=>server.listen(0,'127.0.0.1',yes));
const address=server.address();if(!address||typeof address==='string')throw Error('No fixture port');
const profile=await mkdtemp(join(tmpdir(),'codexify-long-session-'));
if(!resolve(profile).startsWith(resolve(tmpdir())+sep)||!profile.includes('codexify-long-session-'))throw Error('Unexpected disposable profile');
let context:Awaited<ReturnType<typeof chromium.launchPersistentContext>>|undefined;
try {
  const executablePath=await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`));
  context=await chromium.launchPersistentContext(profile,{executablePath,headless:true,ignoreDefaultArgs:true,
    args:[...await backgroundChromeArgs(profile),'--headless=new'],timeout:15000});
  const page=context.pages()[0]??await context.newPage(),session=await context.newCDPSession(page);
  const samples:Array<{index:number;ms:number;heapBytes:number;documents:number;nodes:number}>=[];
  for(let index=0;index<80;index++){
    const start=performance.now();
    await page.goto(`http://127.0.0.1:${address.port}/chat-${index}`,{waitUntil:'domcontentloaded',timeout:5000});
    assert.equal(await page.locator('article').count(),500);
    await maintainRendererMemory(page,index*60_000);
    const [heap,dom]=await Promise.all([session.send('Runtime.getHeapUsage'),session.send('Memory.getDOMCounters')]);
    samples.push({index,ms:Math.round(performance.now()-start),heapBytes:heap.usedSize,documents:dom.documents,nodes:dom.nodes});
  }
  await session.send('HeapProfiler.collectGarbage');
  const finalHeap=await session.send('Runtime.getHeapUsage'),finalDom=await session.send('Memory.getDOMCounters');
  await session.detach();
  const percentile=(values:number[],p:number)=>[...values].sort((a,b)=>a-b)[Math.floor((values.length-1)*p)]!;
  assert.ok(finalHeap.usedSize<128*1024*1024,`Retained heap grew to ${finalHeap.usedSize}`);
  assert.ok(finalDom.documents<20,`Retained ${finalDom.documents} navigation documents`);
  console.log(JSON.stringify({ok:true,switches:samples.length,turnsPerPage:500,medianMs:percentile(samples.map(s=>s.ms),.5),
    first20MedianMs:percentile(samples.slice(0,20).map(s=>s.ms),.5),last20MedianMs:percentile(samples.slice(-20).map(s=>s.ms),.5),
    p95Ms:percentile(samples.map(s=>s.ms),.95),maxHeapBytes:Math.max(...samples.map(s=>s.heapBytes)),finalHeapBytes:finalHeap.usedSize,finalDom}));
} finally {
  await context?.close();
  await rm(profile,{recursive:true,force:true});
  await new Promise<void>((yes,no)=>server.close(error=>error?no(error):yes()));
}
