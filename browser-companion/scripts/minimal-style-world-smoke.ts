import assert from "node:assert/strict";
import {chromium,type BrowserContext} from "patchright";
import {installMinimalChatGptPage,styleRestoredChatGptPage} from "../src/minimal-page";

const browser=await chromium.launch({executablePath:process.env.CODEXIFY_TEST_CHROME_PATH,headless:true});
try {
  const context=await browser.newContext();
  let script="";let argument:unknown;
  await installMinimalChatGptPage({addInitScript:async(fn:unknown,arg:unknown)=>{script=String(fn);argument=arg;}} as unknown as BrowserContext,
    '#probe {width:340px !important;} #probe::before {content:"OLD DEBUG"}',1);
  await context.addInitScript({content:`(${script})(${JSON.stringify(argument)})`});
  await context.route('**/*',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><html><head></head><body><div id="probe">Fixture</div></body></html>'}));
  const page=await context.newPage();await page.goto('https://chatgpt.com/c/style-world-fixture');
  const session=await context.newCDPSession(page);
  const tree=await session.send('Page.getFrameTree');
  const world=await session.send('Page.createIsolatedWorld',{frameId:tree.frameTree.frame.id,worldName:'codexify-style-regression'});
  await installMinimalChatGptPage({addInitScript:async(fn:unknown,arg:unknown)=>{script=String(fn);argument=arg;}} as unknown as BrowserContext,
    '#probe {width:52px !important;}',2);
  const applied=await session.send('Runtime.evaluate',{expression:`(${script})(${JSON.stringify(argument)})`,contextId:world.executionContextId,returnByValue:true});
  assert.equal(applied.exceptionDetails,undefined);
  await page.evaluate(()=>{const child=document.createElement('meta');child.name='fixture-hydration';document.head.append(child);});
  const state=await page.locator('#probe').evaluate(el=>({width:getComputedStyle(el).width,debug:getComputedStyle(el,'::before').content,
    sheets:document.adoptedStyleSheets.filter(sheet=>[...sheet.cssRules].some(rule=>'selectorText' in rule&&rule.selectorText==='#codexify-fast-indicator')).length}));
  assert.equal(state.width,'52px','The latest revision must win across script worlds');
  assert(['none','normal','""'].includes(state.debug),'Obsolete debug CSS must disappear');
  assert.equal(state.sheets,1,'Only one owned adopted stylesheet may remain');
  await styleRestoredChatGptPage(page,'#probe {width:400px !important;}',1);
  assert.equal(await page.locator('#probe').evaluate(el=>getComputedStyle(el).width),'52px','Old controller must not reclaim the shared document');
  await installMinimalChatGptPage({addInitScript:async(fn:unknown,arg:unknown)=>{script=String(fn);argument=arg;}} as unknown as BrowserContext,"",3,"off");
  const disabled=await session.send('Runtime.evaluate',{expression:`(${script})(${JSON.stringify(argument)})`,contextId:world.executionContextId,returnByValue:true});
  assert.equal(disabled.exceptionDetails,undefined);
  await page.evaluate(()=>document.head.append(document.createElement('meta')));
  assert.equal(await page.locator('#codexify-fast-indicator').count(),0,'Disabling must also remove sheets owned by older script worlds');
  assert.equal(await page.evaluate(()=>document.adoptedStyleSheets.length),0);
  await styleRestoredChatGptPage(page,"",4,"bones");
  assert.equal(await page.locator('html').getAttribute('data-codexify-simplicity'),'bones','A newer level may re-enable style after Off');
  assert.equal(await page.locator('#codexify-fast-indicator').textContent(),'CODEXIFY · BONES');
  console.log(JSON.stringify({ok:true,checks:['cross-world replacement','obsolete rules removed','one adopted sheet','hydration preserves latest revision','older world cannot revert','Off removes old-world styles','new level re-enables after Off']}));
} finally {await browser.close();}
