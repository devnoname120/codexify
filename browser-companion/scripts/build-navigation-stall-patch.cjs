// Patch navigation and idle memory maintenance, preserving unrelated bundle code.
const fs = require('node:fs');
const path = require('node:path');
const {createHash, randomUUID} = require('node:crypto');
const ts = require('typescript');
const asar = require('@electron/asar');

function namedFunction(text, filename, name) {
  const tree = ts.createSourceFile(filename, text, ts.ScriptTarget.Latest, true);
  const matches = tree.statements.filter(node => ts.isFunctionDeclaration(node) && node.name?.text === name);
  if (matches.length !== 1) throw new Error('Expected exactly one implementation of ' + name);
  const node = matches[0];
  return {start:node.getStart(tree), end:node.end, text:node.getText(tree)};
}
const navigationFunction = (text, filename) => namedFunction(text, filename, 'navigatePage');

function variableInitializer(text, filename, name) {
  const tree=ts.createSourceFile(filename,text,ts.ScriptTarget.Latest,true);
  const matches=tree.statements.filter(ts.isVariableStatement)
    .flatMap(node=>node.declarationList.declarations).filter(node=>node.name.getText(tree)===name);
  if(matches.length!==1||!matches[0].initializer)throw new Error('Expected exactly one initializer for '+name);
  const node=matches[0].initializer;
  return {start:node.getStart(tree),end:node.end,text:node.getText(tree)};
}

function endActionMethod(text) {
  const tree=ts.createSourceFile('surface.ts',text,ts.ScriptTarget.Latest,true);
  const surface=tree.statements.find(node=>ts.isClassDeclaration(node)&&node.name?.text==='ChromeSingleWindowSurface');
  const method=surface?.members.find(node=>ts.isMethodDeclaration(node)&&node.name?.getText(tree)==='endAction');
  if(!method)throw new Error('Expected the managed surface endAction method');
  return {start:method.getStart(tree),end:method.end,text:method.getText(tree)};
}

function bundleMethod(text,className,name,required=true) {
  const tree=ts.createSourceFile('coordinator.ts',text,ts.ScriptTarget.Latest,true);
  const coordinator=tree.statements.find(node=>ts.isClassDeclaration(node)&&node.name?.text===className);
  if(!coordinator)throw new Error('Expected the single-window coordinator');
  const method=coordinator.members.find(node=>(ts.isMethodDeclaration(node)||ts.isGetAccessor(node))&&node.name?.getText(tree)===name);
  if(!method){if(required)throw new Error('Expected coordinator method '+name);return {start:coordinator.end-1,end:coordinator.end-1,text:''};}
  return {start:method.getStart(tree),end:method.end,text:method.getText(tree)};
}
const coordinatorMethod=(text,name,required=true)=>bundleMethod(text,'SingleWindowCoordinator',name,required);

// A route transplanted from newer source must never call an API missing from
// the installed class. Syntax-only validation cannot catch this mismatch.
function assertRouteCoordinatorCompatibility(text) {
  const tree=ts.createSourceFile('main.js',text,ts.ScriptTarget.Latest,true);
  const coordinator=tree.statements.find(node=>ts.isClassDeclaration(node)&&node.name?.text==='SingleWindowCoordinator');
  const route=tree.statements.find(node=>ts.isFunctionDeclaration(node)&&node.name?.text==='route');
  if(!coordinator||!route)throw Error('Missing coordinator or route for compatibility verification');
  const members=new Set(coordinator.members.filter(node=>ts.isMethodDeclaration(node)||ts.isGetAccessor(node)).map(node=>node.name.getText(tree)));
  const missing=new Set();
  function visit(node){
    if(ts.isCallExpression(node)&&ts.isPropertyAccessExpression(node.expression)
      &&ts.isIdentifier(node.expression.expression)&&node.expression.expression.text==='coordinator'
      &&!members.has(node.expression.name.text))missing.add(node.expression.name.text);
    ts.forEachChild(node,visit);
  }
  visit(route);
  if(missing.size)throw Error('Patched route requires missing coordinator methods: '+[...missing].join(', '));
}

async function main() {
  const root = path.resolve(__dirname, '..');
  const version = fs.readFileSync(path.join(root, '../Cargo.toml'), 'utf8').match(/^version\s*=\s*"([^"]+)"/m)[1];
  const live = path.join(process.env.USERPROFILE, '.codexify/browser/runtime', version, 'app/resources/app.asar');
  const before = asar.extractFile(live, 'dist/main.js').toString();
  if(!/function stopControls\s*\(/.test(before))throw new Error('Installed runtime lacks the Stop control helper required by diagnostics');
  const original = navigationFunction(before, 'main.js');
  const source = navigationFunction(fs.readFileSync(path.join(root, 'src/navigation.ts'), 'utf8'), 'navigation.ts');
  const replacement = ts.transpileModule(source.text.replace(/^export\s+/, ''), {
    compilerOptions:{target:ts.ScriptTarget.ES2022, module:ts.ModuleKind.ESNext},
  }).outputText.trim();
  let after = before.slice(0, original.start) + replacement + before.slice(original.end);
  const oldRecovery=variableInitializer(after,'main.js','recoveryScript');
  const newRecovery=variableInitializer(fs.readFileSync(path.join(root,'src/windows-chrome-recovery.ts'),'utf8'),'windows-chrome-recovery.ts','recoveryScript');
  after=after.slice(0,oldRecovery.start)+newRecovery.text+after.slice(oldRecovery.end);
  const oldDiagnostics = namedFunction(after, 'main.js', 'inspectPresentation');
  const diagnostics = namedFunction(fs.readFileSync(path.join(root, 'src/presentation.ts'), 'utf8'), 'presentation.ts', 'inspectPresentation');
  const diagnosticReplacement = ts.transpileModule(diagnostics.text.replace(/^export\s+/, ''), {
    compilerOptions:{target:ts.ScriptTarget.ES2022, module:ts.ModuleKind.ESNext},
  }).outputText.trim();
  after = after.slice(0, oldDiagnostics.start) + diagnosticReplacement + after.slice(oldDiagnostics.end);
  const oldEnd=endActionMethod(after);
  const newEnd=endActionMethod(fs.readFileSync(path.join(root,'src/chrome-surface.ts'),'utf8'));
  const wrapper=ts.transpileModule('class ChromeSingleWindowSurface {'+newEnd.text+'}',{compilerOptions:{target:ts.ScriptTarget.ES2022}}).outputText;
  after=after.slice(0,oldEnd.start)+endActionMethod(wrapper).text+after.slice(oldEnd.end);
  const memory=ts.transpileModule(fs.readFileSync(path.join(root,'src/renderer-memory.ts'),'utf8'),{
    compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext},
  }).outputText.replace(/^import .*;\r?\n/gm,'').replace(/^export /gm,'');
  const startMarker='// BEGIN CODEXIFY IDLE MEMORY\n',endMarker='// END CODEXIFY IDLE MEMORY\n';
  const existingStart=after.indexOf(startMarker),existingEnd=after.indexOf(endMarker);
  if(existingStart>=0){
    if(existingEnd<existingStart)throw new Error('Invalid existing memory patch boundary');
    after=after.slice(0,existingStart)+after.slice(existingEnd+endMarker.length);
  }
  const insertion=navigationFunction(after,'main.js').start;
  after=after.slice(0,insertion)+startMarker+memory+endMarker+after.slice(insertion);
  const coordinatorSource=fs.readFileSync(path.join(root,'src/browser-window.ts'),'utf8');
  const coordinatorMethods=[
    'busy','readyHealth','presentation','setPresentationStyle','approvePendingConversation','login','capabilities',
    'newChatCapabilities','accountPlan','turnHandoffComplete','refreshConnector','reveal',
    'releaseRetainedCleanup','withWindowOperation','runWithNavigationPolicy','emit',
  ];
  for(const name of coordinatorMethods){
    const current=coordinatorMethod(after,name,!['busy','readyHealth','releaseRetainedCleanup'].includes(name));
    const source=coordinatorMethod(coordinatorSource,name);
    const wrapper=ts.transpileModule('class SingleWindowCoordinator {'+source.text+'}',{compilerOptions:{target:ts.ScriptTarget.ES2022}}).outputText;
    after=after.slice(0,current.start)+'\n'+coordinatorMethod(wrapper,name).text+'\n'+after.slice(current.end);
  }
  const sessionGuard=namedFunction(fs.readFileSync(path.join(root,'src/browser-session.ts'),'utf8'),'browser-session.ts','throwIfSessionAttention');
  const oldGuard=namedFunction(after,'main.js','throwIfSessionAttention');
  const guardJs=ts.transpileModule(sessionGuard.text.replace(/^export\s+/,''),{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext}}).outputText;
  after=after.slice(0,oldGuard.start)+guardJs+after.slice(oldGuard.end);
  const driverSource=fs.readFileSync(path.join(root,'src/chatgpt-driver.ts'),'utf8');
  for(const name of ['inspectHealth','approvePendingConversation']){
    const driverMethod=bundleMethod(driverSource,'ChatGptBrowserDriver',name);
    const oldMethod=bundleMethod(after,'ChatGptBrowserDriver',name);
    const driverWrapper=ts.transpileModule('class ChatGptBrowserDriver {'+driverMethod.text+'}',{compilerOptions:{target:ts.ScriptTarget.ES2022}}).outputText;
    after=after.slice(0,oldMethod.start)+bundleMethod(driverWrapper,'ChatGptBrowserDriver',name).text+after.slice(oldMethod.end);
  }
  const oldRoute=namedFunction(after,'main.js','route');
  const routeSource=namedFunction(fs.readFileSync(path.join(root,'src/control-server.ts'),'utf8'),'control-server.ts','route');
  const routeJs=ts.transpileModule(routeSource.text,{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext}}).outputText;
  after=after.slice(0,oldRoute.start)+routeJs+after.slice(oldRoute.end);
  const oldApproval=namedFunction(after,'main.js','approveConversationCard');
  const approvalSource=namedFunction(fs.readFileSync(path.join(root,'src/conversation-approval.ts'),'utf8'),'conversation-approval.ts','approveConversationCard');
  const approvalJs=ts.transpileModule(approvalSource.text.replace(/^export\s+/,''),{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext}}).outputText;
  after=after.slice(0,oldApproval.start)+approvalJs+after.slice(oldApproval.end);
  const approvalMonitor=ts.transpileModule(fs.readFileSync(path.join(root,'src/approval-monitor.ts'),'utf8'),{
    compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext},
  }).outputText.replace(/^import .*;\r?\n/gm,'').replace(/^export /gm,'');
  const monitorStart='// BEGIN CODEXIFY APPROVAL MONITOR\n',monitorEnd='// END CODEXIFY APPROVAL MONITOR\n';
  const oldMonitorStart=after.indexOf(monitorStart),oldMonitorEnd=after.indexOf(monitorEnd);
  if(oldMonitorStart>=0){
    if(oldMonitorEnd<oldMonitorStart)throw Error('Invalid approval monitor boundary');
    after=after.slice(0,oldMonitorStart)+after.slice(oldMonitorEnd+monitorEnd.length);
  }
  after=monitorStart+approvalMonitor+monitorEnd+after;
  const mainAnchor='server = await startControlServer(coordinator, descriptorPath);';
  if(!after.includes(mainAnchor))throw Error('Missing control server startup anchor');
  if(!after.includes('startAutoApprovalMonitor(coordinator, surface, root);'))
    after=after.replace(mainAnchor,mainAnchor+'\n  startAutoApprovalMonitor(coordinator, surface, root);');
  for(const name of ['lstatSync','readFileSync','join','canonicalChatGptConversationUrl','observeReadOnly']){
    if(!before.includes(name))throw Error('Missing approval monitor dependency '+name);
  }
  if (after === before) throw new Error('Navigation patch is already present');
  const syntax = ts.createSourceFile('main.js', after, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  if (syntax.parseDiagnostics.length) throw new Error('Patched bundle has syntax errors');
  assertRouteCoordinatorCompatibility(after);
  const staging = path.join(root, 'artifacts', 'navigation-stall-' + randomUUID());
  fs.mkdirSync(staging, {recursive:true});
  const unpacked = path.join(staging, 'application');
  asar.extractAll(live, unpacked);
  fs.writeFileSync(path.join(unpacked, 'dist/main.js'), after);
  const candidate = path.join(staging, 'app.asar');
  await asar.createPackage(unpacked, candidate);
  if (asar.extractFile(candidate, 'dist/main.js').toString() !== after) throw new Error('Archive verification failed');
  const hash = bytes => createHash('sha256').update(bytes).digest('hex');
  const receipt = {version, live, candidate, previousSha256:hash(fs.readFileSync(live)), sha256:hash(fs.readFileSync(candidate)),
    beforeBundleSha256:hash(before), bundleSha256:hash(after), changedFunctions:['navigatePage','recoveryScript','inspectPresentation','ChromeSingleWindowSurface.endAction','maintainRendererMemory','throwIfSessionAttention','ChatGptBrowserDriver.inspectHealth','ChatGptBrowserDriver.approvePendingConversation','route','approveConversationCard','readAutoApprovalPolicy','startAutoApprovalMonitor','main.startAutoApprovalMonitor',...coordinatorMethods.map(name=>'SingleWindowCoordinator.'+name)]};
  fs.writeFileSync(path.join(staging, 'receipt.json'), JSON.stringify(receipt, null, 2));
  fs.writeFileSync(path.join(root, 'artifacts/navigation-stall-ready.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify(receipt));
}

if (require.main === module) main().catch(error => {console.error(error.message); process.exitCode = 1;});
module.exports = {navigationFunction,assertRouteCoordinatorCompatibility};
