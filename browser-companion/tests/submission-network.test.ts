import {expect,test} from "bun:test";
import {EventEmitter} from "node:events";
import {observeSubmissionNetwork} from "../src/submission-network";
import type {Page} from "patchright";

const request=(url="https://chatgpt.com/backend-api/f/conversation",method="POST")=>({url:()=>url,method:()=>method});
const jsonRejection=(sent:ReturnType<typeof request>,payload:unknown,status=400)=>{
 const body=Buffer.from(JSON.stringify(payload));
 return {request:()=>sent,status:()=>status,
  headerValue:async(name:string)=>name==='content-type'?'application/json':name==='content-length'?String(body.length):null,
  body:async()=>body};
};

test("structured conversation length rejection does not depend on translated message text",async()=>{
 for(const status of [400,413,422])for(const message of ['La conversation est trop longue.','会話が長すぎます。','المحادثة طويلة جدًا.','',undefined]){
  for(const wrap of [(error:object)=>error,(error:object)=>({detail:error}),(error:object)=>({error})]){
   const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request();
   const pending=proof.wait(new AbortController().signal,500);page.emit('request',sent);
   page.emit('response',jsonRejection(sent,wrap({code:'conversation_length_exceeds_limit',message}),status));
   await expect(pending).rejects.toMatchObject({code:'conversation_limit',sendAccepted:false});proof.close();
  }
 }
});

test("message text and unrelated error codes cannot request conversation replacement",async()=>{
 for(const payload of [{message:'conversation_length_exceeds_limit'},{code:'rate_limit_exceeded'},
  {detail:'conversation_length_exceeds_limit'},{error:{message:'The conversation is too long, please start a new one.'}},
  {code:'rate_limit_exceeded',detail:{code:'conversation_length_exceeds_limit'}},
  {message:'例: conversation_length_exceeds_limit',code:'invalid_request_error'}]){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request();
  const pending=proof.wait(new AbortController().signal,500);page.emit('request',sent);page.emit('response',jsonRejection(sent,payload));
  await expect(pending).rejects.toMatchObject({code:'failed',sendAccepted:true});proof.close();
 }
});

test("authentication, rate limits and server failures keep their existing classification",async()=>{
 for(const status of [401,403,429,500]){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request();
  const pending=proof.wait(new AbortController().signal,500);page.emit('request',sent);
  page.emit('response',jsonRejection(sent,{code:'conversation_length_exceeds_limit'},status));
  await expect(pending).rejects.toMatchObject({code:status===429?'rate_limited':'failed',sendAccepted:true});proof.close();
 }
});

test("successful generation streams are acknowledged without reading their bodies",async()=>{
 const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request();let reads=0;
 page.emit('request',sent);page.emit('response',{request:()=>sent,status:()=>200,body:()=>{reads++;throw Error('Do not consume the generation stream');}});
 await proof.wait(new AbortController().signal,100);expect(reads).toBe(0);proof.close();
});

test("malformed, oversized and non-JSON rejection bodies retain the original failure",async()=>{
 for(const mode of ['malformed','oversized','html'] as const){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request();let reads=0;
  const pending=proof.wait(new AbortController().signal,500);page.emit('request',sent);
  page.emit('response',{request:()=>sent,status:()=>400,
   headerValue:async(name:string)=>name==='content-type'?(mode==='html'?'text/html':'application/json'):name==='content-length'&&mode==='oversized'?'65537':null,
   body:async()=>{reads++;return Buffer.from(mode==='malformed'?'{broken':JSON.stringify({code:'conversation_length_exceeds_limit'}));}});
  await expect(pending).rejects.toMatchObject({code:'failed',sendAccepted:true});
  expect(reads).toBe(mode==='malformed'?1:0);proof.close();
 }
});

test("a stalled rejection body cannot hold submission classification indefinitely",async()=>{
 const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request();let reads=0;
 const pending=proof.wait(new AbortController().signal,2500);page.emit('request',sent);
 page.emit('response',{request:()=>sent,status:()=>400,headerValue:async(name:string)=>name==='content-type'?'application/json':null,
  body:()=>{reads++;return new Promise(()=>{});}});
 await expect(pending).rejects.toMatchObject({code:'failed',sendAccepted:true});expect(reads).toBe(1);proof.close();
});

test("a structured length error from another request cannot replace this conversation",async()=>{
 for(const url of ['https://example.com/backend-api/f/conversation','https://chatgpt.com/backend-api/f/conversation/prepare']){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page),sent=request(url);
  const pending=proof.wait(new AbortController().signal,25);page.emit('request',sent);
  page.emit('response',jsonRejection(sent,{code:'conversation_length_exceeds_limit'}));
  await expect(pending).rejects.toMatchObject({code:'ambiguous_send',sendAccepted:true});proof.close();
 }
});

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

test("an owner message can be acknowledged by its own successful steering response",async()=>{
 for(const path of ['/backend-api/f/steer_turn','/backend-api/steer_turn']){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page,{allowSteering:true});
  const pending=proof.wait(new AbortController().signal,100),steer=request('https://chatgpt.com'+path);
  let settled=false;void pending.then(()=>{settled=true;},()=>{settled=true;});
  page.emit('request',steer);await Promise.resolve();expect(settled).toBe(false);
  page.emit('response',{request:()=>request('https://chatgpt.com'+path),status:()=>200});
  await Promise.resolve();expect(settled).toBe(false);
  page.emit('response',{request:()=>steer,status:()=>200});
  expect(await pending).toBe('steering');proof.close();
 }
});

test("failed owner-message steering retains server failure and uncertain delivery",async()=>{
 for(const code of [400,429,500]){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page,{allowSteering:true});
  const pending=proof.wait(new AbortController().signal,100),steer=request('https://chatgpt.com/backend-api/f/steer_turn');
  page.emit('request',steer);page.emit('response',jsonRejection(steer,{code:'conversation_length_exceeds_limit'},code));
  await expect(pending).rejects.toMatchObject({code:code===429?'rate_limited':'failed',sendAccepted:true,message:`ChatGPT's steering submission returned HTTP ${code}`});proof.close();
 }
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

test("HTTP 413 on a small continuation requests a fresh conversation without claiming acceptance",async()=>{
 for(const continuation of [false,true]){
  const page=new EventEmitter(),proof=observeSubmissionNetwork(page as unknown as Page,{recoverOversizedContinuation:continuation});
  const pending=proof.wait(new AbortController().signal,100),sent=request();
  page.emit('request',sent);page.emit('response',{request:()=>sent,status:()=>413});
  await expect(pending).rejects.toMatchObject(continuation
    ?{code:'conversation_limit',sendAccepted:false}
    :{code:'failed',sendAccepted:true});
  proof.close();
 }
});
