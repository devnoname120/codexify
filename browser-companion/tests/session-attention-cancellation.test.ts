import {expect,test} from 'bun:test';
import type {Page} from 'patchright';
import {throwIfSessionAttention} from '../src/browser-session';

test('a cancelled session probe cannot start later UI reads after a delayed result',async()=>{
  let reads=0,release!:(value:boolean)=>void;
  const held=new Promise<boolean>(resolve=>release=resolve);
  const locator={filter:()=>locator,last:()=>locator,isVisible:()=>{reads++;return held;}};
  const page={context:()=>({}),locator:()=>locator,url:()=> 'https://chatgpt.com/c/fixture'} as unknown as Page;
  await expect(throwIfSessionAttention(page,AbortSignal.timeout(20))).rejects.toThrow();
  release(false);await Bun.sleep(10);expect(reads).toBe(1);
});
