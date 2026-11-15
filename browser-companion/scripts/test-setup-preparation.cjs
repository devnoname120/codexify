// Run the real coordinator under Node, the packaged companion's runtime.
const assert = require('node:assert/strict');
const {test} = require('node:test');
const {registerHooks} = require('node:module');
const {readFileSync} = require('node:fs');
const {pathToFileURL, fileURLToPath} = require('node:url');
const path = require('node:path');
const ts = require('typescript');
registerHooks({
  resolve(specifier, context, next) {
    try { return next(specifier, context); }
    catch (error) {
      if (specifier.startsWith('.') && !path.extname(specifier)) return next(specifier + '.ts', context);
      throw error;
    }
  },
  load(url, context, next) {
    if (['/browser-companion/model-fallback-order.json','/browser-companion/ui-locales.json'].some(suffix=>url.endsWith(suffix))) return {format:'module', shortCircuit:true,
      source:'export default ' + JSON.stringify(JSON.parse(readFileSync(fileURLToPath(url),'utf8'))) + ';'};
    if (url.endsWith('.ts')) return {format:'module', shortCircuit:true,
      source:ts.transpileModule(readFileSync(fileURLToPath(url), 'utf8'), {
        compilerOptions:{target:ts.ScriptTarget.ES2022, module:ts.ModuleKind.ESNext},
      }).outputText};
    return next(url, context);
  },
});
const implementation = import(pathToFileURL(path.resolve(__dirname, '../src/browser-window.ts')).href);
const protocol = import(pathToFileURL(path.resolve(__dirname, '../src/protocol.ts')).href);
async function fixture(overrides = {}) {
  const {SingleWindowCoordinator} = await implementation;
  const {protocolBuild} = await protocol;
  const build = protocolBuild('setup-preparation-regression');
  const catalog = {protocolVersion:1, build, accountFingerprint:'fixture-account', catalogRevision:'fixture', models:[]};
  const driver = {
    async health(){return {authenticated:true, accountFingerprint:'fixture-account', status:'idle', detail:null};},
    async prepareNewChat(){return catalog;},
    async capabilities(){return catalog;},
    async execute(){throw Error('Unexpected Send');},
    async close(){}, ...overrides,
  };
  const coordinator = new SingleWindowCoordinator(driver, build, {operationMs:80, cleanupMs:20, hardStopMs:20});
  return {coordinator, catalog};
}
test('cold navigation plus discovery may exceed a maintenance deadline without losing setup', async()=>{
  let finished = false;
  const {coordinator} = await fixture({async prepareNewChat(_project, signal){
    await new Promise(resolve=>setTimeout(resolve,130)); signal.throwIfAborted(); finished=true;
    return {protocolVersion:1, accountFingerprint:'fixture-account', catalogRevision:'fixture', models:[]};
  }});
  try {assert.equal((await coordinator.newChatCapabilities(null)).accountFingerprint,'fixture-account'); assert(finished);}
  finally {await coordinator.close();}
});
test('a stalled setup remains bounded and a fresh request can recover', async()=>{
  let attempts=0;
  const {coordinator,catalog} = await fixture({async prepareNewChat(_project, signal){
    if(++attempts>1)return catalog;
    await new Promise((_,reject)=>signal.addEventListener('abort',()=>reject(signal.reason),{once:true}));
  }});
  try {
    await assert.rejects(coordinator.newChatCapabilities(null), /timed out/);
    assert.equal((await coordinator.newChatCapabilities(null)).accountFingerprint,'fixture-account');
  } finally {await coordinator.close();}
});
test('cancelling setup releases ownership without submitting a message', async()=>{
  const abort=new AbortController(); let started;
  const entered=new Promise(resolve=>{started=resolve;});
  const {coordinator}=await fixture({async prepareNewChat(_project, signal){
    started(); await new Promise((_,reject)=>signal.addEventListener('abort',()=>reject(signal.reason),{once:true}));
  }});
  try {
    const pending=coordinator.newChatCapabilities(null,abort.signal);await entered;abort.abort();
    await assert.rejects(pending); assert.equal((await coordinator.readyHealth()).status,'idle');
  } finally {await coordinator.close();}
});
test('a changed account still prevents publishing the prepared catalogue',async()=>{
  const {coordinator}=await fixture({async health(){return {authenticated:true,accountFingerprint:'different-account',status:'idle',detail:null};}});
  try {await assert.rejects(coordinator.newChatCapabilities(null), /account or browser changed/);}
  finally {await coordinator.close();}
});
test('plain user messages retain contrast and diagnostics recognize active response controls',
  {skip:!process.env.CODEXIFY_TEST_CHROME_PATH},async()=>{
  const {chromium}=require('patchright');
  const {installMinimalChatGptPage,styleRestoredChatGptPage}=await import(pathToFileURL(path.resolve(__dirname,'../src/minimal-page.ts')).href);
  const {inspectPresentation}=await import(pathToFileURL(path.resolve(__dirname,'../src/presentation.ts')).href);
  const browser=await chromium.launch({executablePath:process.env.CODEXIFY_TEST_CHROME_PATH,headless:true});
  try {
    const context=await browser.newContext();
    await context.route('**/*',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><style>:root{--text-primary:#222}body{background:white;color:var(--text-primary)}[data-user-message-bubble]{background:purple;color:white}</style><main><div data-user-message-bubble>Fixture user message</div><form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true"></div></form></main>'}));
    await installMinimalChatGptPage(context,'',0,'bones');
    const page=await context.newPage();await page.goto('https://chatgpt.com/c/contrast-fixture');
    const colors=()=>page.locator('[data-user-message-bubble]').evaluate(el=>({color:getComputedStyle(el).color,background:getComputedStyle(el).backgroundColor}));
    assert.deepEqual(await colors(),{color:'rgb(34, 34, 34)',background:'rgba(0, 0, 0, 0)'});
    await page.evaluate(()=>{document.documentElement.style.setProperty('--text-primary','#eee');document.body.style.background='black';});
    assert.equal((await colors()).color,'rgb(238, 238, 238)');
    await styleRestoredChatGptPage(page,'',1,'off');
    assert.deepEqual(await colors(),{color:'rgb(255, 255, 255)',background:'rgb(128, 0, 128)'});
    assert.equal((await inspectPresentation(page)).generationRunning,false);
    await page.locator('form').evaluate(form=>{const stop=document.createElement('button');stop.type='button';stop.dataset.testid='stop-button';stop.setAttribute('aria-label','End turn');form.append(stop);});
    const active=await inspectPresentation(page);
    assert.equal(active.generationRunning,true);assert.equal(active.observedActionPages,1);
    await page.locator('[data-testid="stop-button"]').evaluate(button=>{button.hidden=true;});
    assert.equal((await inspectPresentation(page)).generationRunning,false,'A hidden stale Stop must not block an update');
  } finally {await browser.close();}
});
