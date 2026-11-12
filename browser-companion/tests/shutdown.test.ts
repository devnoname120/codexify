import {expect,test} from 'bun:test';
import {shutdownWithDeadline} from '../src/shutdown';

test('hung shutdown reaches the exit deadline',async()=>{
 let exits=0;
 await shutdownWithDeadline(()=>new Promise(()=>{}),()=>{exits++;},15);
 expect(exits).toBe(1);
});
test('clean shutdown cancels its exit deadline',async()=>{
 let exits=0;
 await shutdownWithDeadline(async()=>{},()=>{exits++;},15);
 await Bun.sleep(30);
 expect(exits).toBe(0);
});
test('failed cleanup cancels its deadline and propagates the error',async()=>{
 let exits=0;
 await expect(shutdownWithDeadline(async()=>{throw Error('cleanup failed');},()=>{exits++;},15)).rejects.toThrow('cleanup failed');
 await Bun.sleep(30);
 expect(exits).toBe(0);
});
