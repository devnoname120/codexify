import {expect,test} from "bun:test";
import {createServer} from "node:http";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {chromium,type BrowserContext} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {backgroundChromeArgs} from "../src/background-chrome";
import {parseRequestBlockList,prepareRequestBlockList,requestBlockListArgs,verifyRequestBlockList} from "../src/request-block-list";

test("request block list input is bounded and empty defaults do not change launch arguments",async()=>{
 expect(parseRequestBlockList(undefined)).toEqual([]);
 expect(parseRequestBlockList('["^https?://example\\\\.com/"]')).toEqual(['^https?://example\\.com/']);
 for(const invalid of ['{','null','{}','[1]','[""]',JSON.stringify(['é']),JSON.stringify(['x'.repeat(4097)]),JSON.stringify(Array(129).fill('x'))])expect(()=>parseRequestBlockList(invalid)).toThrow();
 expect(await prepareRequestBlockList('/unused/profile',[])).toBeUndefined();
 expect(requestBlockListArgs(['--disable-extensions','--no-startup-window'],undefined)).toEqual(['--disable-extensions','--no-startup-window']);
});

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=="1")("browser-wide regex blocking sends zero matching requests, including redirect hops and workers",async()=>{
 const root=await mkdtemp(join(tmpdir(),'request-block-list-')),hits:string[]=[];
 const server=createServer((request,response)=>{
  hits.push(request.url||'');const url=new URL(request.url||'/',`http://${request.headers.host}`);
  if(url.pathname==='/redirect'){response.writeHead(302,{location:url.searchParams.get('to')!});response.end();return;}
  if(url.pathname==='/worker.js'){response.writeHead(200,{'content-type':'application/javascript'});response.end("onmessage=async e=>{try{await fetch(e.data);postMessage('allowed')}catch{postMessage('blocked')}};");return;}
  if(url.pathname==='/sw.js'){response.writeHead(200,{'content-type':'application/javascript'});response.end("self.addEventListener('install',()=>self.skipWaiting());self.addEventListener('activate',e=>e.waitUntil(self.clients.claim()));self.addEventListener('message',e=>{e.waitUntil(fetch(e.data).then(()=>e.ports[0].postMessage('allowed'),()=>e.ports[0].postMessage('blocked')))});");return;}
  response.writeHead(200,{'content-type':'text/html'});response.end('<html><body>Allowed response</body></html>');
 });
 await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));
 const port=(server.address() as {port:number}).port,base=`http://127.0.0.1:${port}`;
 const patterns=[`^https?://127\\.0\\.0\\.1:${port}/.*/rgstr`,`^https?://127\\.0\\.0\\.1:${port}/backend-api/.*/conversations(?:\\?|$)`,`^https?://127\\.0\\.0\\.1:${port}/backend-api/conversations(?:\\?|$)`];
 const examples=['^https?://(www\\.)?chatgpt\\.com/.*/rgstr','^https?://(www\\.)?chatgpt\\.com/backend-api/.*/conversations(?:\\?|$)','^https?://(www\\.)?chatgpt\\.com/backend-api/conversations(?:\\?|$)'];
 const executablePath=await privateChromeExecutable(join(import.meta.dirname,'../.browsers',`${process.platform}-${process.arch}`)),profile=join(root,'profile');let context:BrowserContext|undefined;
 const launch=async(list:readonly string[])=>{
  const policy=await prepareRequestBlockList(profile,list);
  const current=await chromium.launchPersistentContext(profile,{channel:'chrome',executablePath,headless:true,viewport:null,ignoreDefaultArgs:true,args:[...requestBlockListArgs(await backgroundChromeArgs(profile),policy),'--headless=new']});
  try{await verifyRequestBlockList(current,policy);return current;}catch(error){await current.close();throw error;}
 };
 try {
  context=await launch([...patterns,...examples]);const page=await context.newPage();await page.goto(base+'/allowed');
  const urls=['/events/rgstr','/backend-api/accounts/account/conversations','/backend-api/conversations'];
  for(const path of urls){
   expect(await page.evaluate(async url=>{try{await fetch(url,{method:'POST',body:'must never reach the server'});return 'allowed';}catch{return 'blocked';}},path)).toBe('blocked');
   expect(await page.evaluate(async url=>{try{await fetch('/redirect?to='+encodeURIComponent(url));return 'allowed';}catch{return 'blocked';}},path)).toBe('blocked');
  }
  expect(await page.evaluate(async()=> (await fetch('/events/RGSTR')).status)).toBe(200);
  for(const path of ['/backend-api/conversation','/backend-api/conversations/chat-id?num_turns=10','/backend-api/accounts/account/conversations/chat-id']){
   expect(await page.evaluate(async path=> (await fetch(path)).status,path)).toBe(200);
   expect(await page.evaluate(async path=> (await fetch(path,{method:'POST',body:'allowed'})).status,path)).toBe(200);
  }
  expect(await page.evaluate(async()=>{try{await fetch('/backend-api/conversations?limit=20');return 'allowed';}catch{return 'blocked';}})).toBe('blocked');
  const navigation=await context.newPage();await expect(navigation.goto(base+'/redirect?to='+encodeURIComponent(urls[0]!))).rejects.toThrow('ERR_BLOCKED_BY_CLIENT');await navigation.close();
  const popupPromise=context.waitForEvent('page');await page.evaluate(path=>window.open('/redirect?to='+encodeURIComponent(path),'_blank'),urls[1]!);const popup=await popupPromise;
  await popup.waitForLoadState('domcontentloaded').catch(()=>{});await popup.close();
  await page.evaluate(path=>{const frame=document.createElement('iframe');frame.src='/redirect?to='+encodeURIComponent(path);document.body.append(frame);const image=new Image();image.src=path;document.body.append(image);navigator.sendBeacon(path,'blocked beacon');},urls[2]!);
  expect(await page.evaluate(path=>new Promise<string>(resolve=>{const worker=new Worker('/worker.js');worker.onmessage=e=>{resolve(e.data);worker.terminate();};worker.postMessage('/redirect?to='+encodeURIComponent(path));}),urls[0]!)).toBe('blocked');
  expect(await page.evaluate(async path=>{const registration=await navigator.serviceWorker.register('/sw.js');const installing=registration.installing||registration.waiting||registration.active!;if(installing.state!=='activated')await new Promise<void>(resolve=>installing.addEventListener('statechange',()=>{if(installing.state==='activated')resolve();}));return await new Promise<string>(resolve=>{const channel=new MessageChannel();channel.port1.onmessage=e=>resolve(e.data);registration.active!.postMessage(path,[channel.port2]);});},urls[2]!)).toBe('blocked');
  expect(hits.some(url=>urls.includes(new URL(url,base).pathname))).toBeFalse();
  const contextPolicy=await prepareRequestBlockList(profile,[`^${base}/other-blocked`]);
  await context.close();context=await launch([`^${base}/other-blocked`]);const changed=await context.newPage();await changed.goto(base+urls[2]!);
  await context.close();context=await launch([]);const unfiltered=await context.newPage();await unfiltered.goto(base+'/other-blocked');
  expect(hits.includes('/other-blocked')).toBeTrue();expect(contextPolicy!.count).toBe(1);
  await context.close();context=undefined;
  await expect(launch(['(?=not-supported)'])).rejects.toThrow('not supported by Chromium RE2');
 }finally{await context?.close();await new Promise<void>(resolve=>server.close(()=>resolve()));await rm(root,{recursive:true,force:true});}
},60000);
