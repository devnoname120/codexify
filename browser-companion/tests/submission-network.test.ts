import {expect,test} from "bun:test";
import {EventEmitter} from "node:events";
import {observeSubmissionNetwork} from "../src/submission-network";
import type {Page} from "patchright";

const request=(url="https://chatgpt.com/backend-api/f/conversation",method="POST")=>({url:()=>url,method:()=>method});

test("submission needs its own server acknowledgement, not optimistic DOM or unrelated requests",async()=>{
 const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),signal=new AbortController().signal;
 const pending=proof.wait(signal,25);
 const old=request();page.emit('response',{request:()=>old,status:()=>200});
 for(const url of ['https://chatgpt.com/backend-api/f/conversation/prepare','https://chatgpt.com/backend-api/telemetry','https://example.com/backend-api/f/conversation']){
  const other=request(url);page.emit('request',other);page.emit('response',{request:()=>other,status:()=>200});
 }
 await expect(pending).rejects.toMatchObject({code:'ambiguous_send',sendAccepted:true});
 proof.close();expect(page.listenerCount('response')).toBe(0);
});

test("the conversation submission's successful response releases the send lease",async()=>{
 const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page);
 const sent=request();page.emit('request',sent);page.emit('response',{request:()=>sent,status:()=>200});
 await proof.wait(new AbortController().signal,50);proof.close();
});

test("steering an existing turn cannot acknowledge a new continuation",async()=>{
 const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page);
 const pending=proof.wait(new AbortController().signal,50),steer=request('https://chatgpt.com/backend-api/f/steer_turn');
 page.emit('request',steer);page.emit('response',{request:()=>steer,status:()=>200});
 await expect(pending).rejects.toMatchObject({code:'ambiguous_send',sendAccepted:true});
 proof.close();
});

test("server rejection, failed transport and cancellation preserve uncertain Send",async()=>{
 for(const mode of ['429','failed','cancel'] as const){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),abort=new AbortController();
  const pending=proof.wait(abort.signal,100),sent=request();page.emit('request',sent);
  if(mode==='429')page.emit('response',{request:()=>sent,status:()=>429});
  else if(mode==='failed')page.emit('requestfailed',sent);
  else abort.abort(new Error('cancelled'));
  await expect(pending).rejects.toThrow(mode==='429'?'HTTP 429':mode==='failed'?'before server acceptance':'cancelled');
  proof.close();expect(page.listenerCount('request')).toBe(0);
 }
});
