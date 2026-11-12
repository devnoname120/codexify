// Isolated real-Chrome regression: no user profile or ChatGPT session is opened.
import assert from 'node:assert/strict';
import {chromium} from 'patchright';
import {SingleWindowCoordinator,type BrowserDriver} from '../src/browser-window';
import {protocolBuild,type CapabilityCatalog} from '../src/protocol';

async function main(){
 const browser=await chromium.launch({executablePath:process.env.CODEXIFY_TEST_CHROME,headless:true});
 const context=await browser.newContext(),page=await context.newPage();
 const build=protocolBuild('maintenance-observation-smoke');let calls=0,closed=0;
 const catalog:CapabilityCatalog={protocolVersion:1,build,accountFingerprint:'fixture',catalogRevision:'fixture',models:[]};
 const driver:BrowserDriver={
  async health(){return {authenticated:true,accountFingerprint:'fixture',status:'idle',detail:null};},
  async liveness(){return page.evaluate(()=>true);},
  async login(){throw Error('Unexpected login');},
  async capabilities(){
   if(++calls===1)await page.evaluate(()=>new Promise<void>(resolve=>{(globalThis as any).releaseObservation=resolve;}));
   return catalog;
  },
  async reveal(){},async execute(){throw Error('Unexpected Send');},
  async close(){closed++;await browser.close();},
 };
 const budget={operationMs:150,cleanupMs:50,hardStopMs:40};
 const coordinator=new SingleWindowCoordinator(driver,build,budget);
 try{
  await page.setContent('<title>Disposable maintenance regression</title>');
  await context.addCookies([{name:'fixture_cookie',value:'fake',url:'http://127.0.0.1'}]);
  await assert.rejects(coordinator.capabilities(),/timed out/u);
  assert.equal(closed,0);assert(browser.isConnected());assert(!page.isClosed());
  assert.equal((await coordinator.health()).status,'busy');assert((await coordinator.liveness()).available);
  await assert.rejects(coordinator.capabilities(),/busy/u);assert.equal(calls,1);
  await page.evaluate(()=>{(globalThis as any).releaseObservation();});
  await new Promise(resolve=>setTimeout(resolve,20));
  assert.equal((await coordinator.capabilities()).accountFingerprint,'fixture');assert.equal(calls,2);
  assert.equal((await context.cookies('http://127.0.0.1'))[0]?.name,'fixture_cookie');
  assert.equal(closed,0);
  console.log('REAL_CHROME_OBSERVATION_PRESERVATION_OK');
 }finally{
  await page.evaluate(()=>{(globalThis as any).releaseObservation?.();}).catch(()=>{});
  budget.operationMs=5000; // Real process shutdown is outside the scaled observation deadline.
  await coordinator.close();await browser.close();
 }
}
void main().catch(error=>{console.error(error);process.exitCode=1;});
