import {createServer} from "node:http";
import assert from "node:assert/strict";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join,resolve,sep} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {backgroundChromeArgs} from "../src/background-chrome";

// Isolated local pages only: no managed profile, account or running agent.
const server=createServer((_request,response)=>{
  response.setHeader("Content-Type","text/html");
  response.end(`<!doctype html><title>navigation cache fixture</title><script>
    document.documentElement.dataset.documentId=crypto.randomUUID();
    window.retained=Array.from({length:250000},(_,i)=>i);
    addEventListener('pageshow',event=>document.documentElement.dataset.restored=String(event.persisted));
  </script><body><form><textarea></textarea><button>Send</button></form></body>`);
});
await new Promise<void>(yes=>server.listen(0,"127.0.0.1",yes));
const address=server.address();
if(!address||typeof address==="string")throw new Error("Missing local fixture port");
const origin=`http://127.0.0.1:${address.port}`;
const executablePath=await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const results=[];
try {
  for(const disableCache of [false,true]) {
    const profile=await mkdtemp(join(tmpdir(),"codexify-navigation-memory-"));
    if(!resolve(profile).startsWith(resolve(tmpdir())+sep)||!profile.includes("codexify-navigation-memory-"))throw new Error("Unsafe fixture profile");
    let context:Awaited<ReturnType<typeof chromium.launchPersistentContext>>|undefined;
    try {
      const args=await backgroundChromeArgs(profile);
      // Measure the old policy independently after the production fix lands.
      const baseline=args.filter(arg=>arg!=="--disable-back-forward-cache");
      context=await chromium.launchPersistentContext(profile,{executablePath,
        headless:true,ignoreDefaultArgs:true,timeout:15000,
        args:[...baseline,"--headless=new",...(disableCache?["--disable-back-forward-cache"]:[])]});
      const page=context.pages()[0]??await context.newPage();
      await page.goto(`${origin}/a`);
      const original=await page.evaluate(()=>document.documentElement.dataset.documentId);
      await page.goto(`${origin}/b`);
      const session=await context.newCDPSession(page);
      await session.send("HeapProfiler.collectGarbage");
      const counters=await session.send("Memory.getDOMCounters");
      await session.detach();
      // A BFCache restore does not emit a new load event. Observe its exact
      // target without imposing a load-event timeout on the baseline.
      await page.evaluate(()=>history.back());
      await page.waitForURL(`${origin}/a`,{waitUntil:"commit",timeout:5000});
      const result=await page.evaluate(()=>({sameDocument:document.documentElement.dataset.documentId,restored:document.documentElement.dataset.restored}));
      if(!original||!result.sameDocument)throw new Error("Fixture document identity missing");
      results.push({disableCache,reusedPreviousDocument:result.sameDocument===original,persisted:result.restored,retainedDocuments:counters.documents});
    } finally {await context?.close();await rm(profile,{recursive:true,force:true});}
  }
  assert.equal(results[1]?.reusedPreviousDocument,false);
  assert.equal(results[1]?.persisted,"false");
  // If this Chrome supports BFCache under automation, verify actual retained
  // document reduction as well as the fresh navigation identity.
  if(results[0]?.persisted==="true")assert.ok(results[1]!.retainedDocuments<results[0]!.retainedDocuments);
  console.log(JSON.stringify({navigationCache:results}));
} finally {await new Promise<void>((yes,no)=>server.close(error=>error?no(error):yes()));}
