// Real Windows Chrome persistence test; uses only disposable profiles and fake cookies.
const fs=require('node:fs'),path=require('node:path'),assert=require('node:assert/strict');
const {execFileSync}=require('node:child_process');
const {chromium}=require('patchright');
const core=require('patchright-core/lib/coreBundle');
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const ps=code=>execFileSync('powershell.exe',['-NoProfile','-NonInteractive','-Command',code],{encoding:'utf8',windowsHide:true}).trim();
(async()=>{
 const meta=JSON.parse(fs.readFileSync(path.resolve(__dirname,'../.browsers',process.platform+'-'+process.arch,'chrome-executable.json'),'utf8'));
 const executablePath=path.resolve(__dirname,'../.browsers',process.platform+'-'+process.arch,meta.executable);
 const server=core.server.createPlaywright({sdkLanguage:'javascript'});
 const results=[];
 for(const graceful of [false,true]){
  const profile=path.join('D:/CodexifyBuild/team-controls-20261025','session-fixture-'+Date.now()+'-'+Number(graceful));fs.mkdirSync(profile,{recursive:true});
  const generated=await server.chromium.defaultArgs({channel:'chrome',headless:false},true,profile);
  const args=[...generated.filter(arg=>arg!=='about:blank'),'--no-startup-window','--start-minimized'];
  const launch=()=>chromium.launchPersistentContext(profile,{channel:'chrome',executablePath,headless:false,ignoreDefaultArgs:true,args,timeout:30000});
  let context=await launch();
  try{
   if(!context.pages().length){const cdp=await context.browser().newBrowserCDPSession();await Promise.all([context.waitForEvent('page'),cdp.send('Target.createTarget',{url:'about:blank',newWindow:true,background:true})]);await cdp.detach();}
   await context.addCookies([{name:'fixture_session',value:'fake-session',url:'http://127.0.0.1',httpOnly:true},{name:'fixture_persistent',value:'fake-persistent',url:'http://127.0.0.1',expires:Math.floor(Date.now()/1000)+3600,httpOnly:true}]);
   const escaped=profile.replaceAll("'","''");
   const root=JSON.parse(ps("$p=Get-CimInstance Win32_Process|Where-Object {$_.Name -eq 'chrome.exe' -and $_.CommandLine.Contains('"+escaped+"') -and $_.CommandLine -notlike '*--type=*'};if(@($p).Count -ne 1){throw 'Fixture Chrome identity is ambiguous'};$p|Select-Object ProcessId,ExecutablePath|ConvertTo-Json -Compress"));
   assert.equal(path.resolve(root.ExecutablePath).toLowerCase(),executablePath.toLowerCase());
   const first=context;const onPageClosed=()=>{if(first.pages().every(page=>page.isClosed()))void first.close().catch(()=>{});};for(const page of first.pages())page.once('close',onPageClosed);first.on('page',page=>page.once('close',onPageClosed));
   const closed=context.waitForEvent('close',{timeout:15000});
   if(graceful){const sent=ps("$p=Get-Process -Id "+root.ProcessId+";$p.Refresh();if(-not $p.MainWindowHandle){throw 'Fixture has no closeable window'};$p.CloseMainWindow()");assert.equal(sent,'True');}else{execFileSync('taskkill.exe',['/PID',String(root.ProcessId),'/T','/F'],{windowsHide:true});}
   await closed;context=undefined;
   for(let i=0;i<100;i++){if(ps('if(Get-Process -Id '+root.ProcessId+' -ErrorAction SilentlyContinue){"running"}else{"closed"}')==='closed')break;await delay(100);}
   context=await launch();const names=(await context.cookies('http://127.0.0.1')).map(cookie=>cookie.name);
   if(graceful)assert(names.includes('fixture_persistent'),'persistent cookie survives clean close');
   results.push({graceful,sessionSurvived:names.includes('fixture_session'),persistentSurvived:names.includes('fixture_persistent')});
  }finally{await context?.close();}
 }
 console.log(JSON.stringify(results));assert(results.find(row=>row.graceful).persistentSurvived,'clean close must preserve persistent login cookies');
 console.log('WINDOWS_SESSION_PERSISTENCE_SMOKE_OK');
})().catch(error=>{console.error(error);process.exitCode=1;});
