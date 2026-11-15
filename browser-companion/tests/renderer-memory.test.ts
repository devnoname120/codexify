import {expect,test} from 'bun:test';
import type {Page} from 'patchright';
import {maintainRendererMemory} from '../src/renderer-memory';

function fixture(usedSize=1024*1024*1024) {
  const calls:string[]=[];let detached=0;
  const session={send:async(method:string)=>{calls.push(method);if(method==='HeapProfiler.collectGarbage')usedSize=128*1024*1024;return {usedSize};},detach:async()=>{detached++;}};
  const page={isClosed:()=>false,context:()=>({newCDPSession:async()=>session})} as unknown as Page;
  return {page,session,calls,detached:()=>detached};
}
test('idle heap cleanup releases unreachable memory without browser data deletion',async()=>{
  const f=fixture();await maintainRendererMemory(f.page,1_000);
  expect(f.calls).toEqual(['Runtime.getHeapUsage','HeapProfiler.collectGarbage','Runtime.getHeapUsage']);
  expect(f.detached()).toBe(1);
});
test('small heaps skip collection and repeated completions are throttled',async()=>{
  const f=fixture(100);await maintainRendererMemory(f.page,1_000);
  for(let i=0;i<100;i++)await maintainRendererMemory(f.page,1_001+i);
  expect(f.calls).toEqual(['Runtime.getHeapUsage']);expect(f.detached()).toBe(1);
  await maintainRendererMemory(f.page,61_000);expect(f.calls).toHaveLength(2);
});
test('a failed collection preserves the action outcome and detaches its session',async()=>{
  const f=fixture();f.session.send=async method=>{if(method==='HeapProfiler.collectGarbage')throw Error('Page changed');return {usedSize:1024*1024*1024};};
  await maintainRendererMemory(f.page,1_000);expect(f.detached()).toBe(1);
});
test('memory checks stay separate between pages and closed pages are skipped',async()=>{
  const first=fixture(100),second=fixture(100);
  await maintainRendererMemory(first.page,1_000);await maintainRendererMemory(second.page,1_001);
  expect(second.calls).toEqual(['Runtime.getHeapUsage']);
  const closed=fixture();closed.page.isClosed=()=>true;await maintainRendererMemory(closed.page,1_000);expect(closed.calls).toEqual([]);
});
test('unsupported or invalid heap metrics cannot trigger garbage collection',async()=>{
  for(const value of [undefined,NaN,Infinity]){
    const f=fixture();f.session.send=async method=>{f.calls.push(method);return {usedSize:value as number};};
    await maintainRendererMemory(f.page,1_000);expect(f.calls).toEqual(['Runtime.getHeapUsage']);expect(f.detached()).toBe(1);
  }
});
