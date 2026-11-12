#Requires -Version 7.0
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-agent-update-guards.ps1')
$app='C:\fixture\.codexify'
$cua='C:\fixture\runtimes\cua_node'
function Row($Id,$Parent,$Name,$Exe,$Command){[pscustomobject]@{ProcessId=$Id;ParentProcessId=$Parent;Name=$Name;ExecutablePath=$Exe;CommandLine=$Command}}
function Check($Rows,$Expected,$Label){
  $actual=@(Get-AgentUpdateWorkProcesses -Processes $Rows -ServerId 10 -BrowserId 20 -AppRoot $app -CuaRuntimeRoot $cua -SystemRoot 'C:\Windows'|ForEach-Object ProcessId|Sort-Object)
  if(($actual -join ',') -ne (@($Expected|Sort-Object) -join ',')){throw "$Label failed: expected $Expected, got $actual"}
}
$base=@(
  (Row 11 10 'node.exe' "$cua\v1\bin\node.exe" "node.exe C:/runtime/@oai/cua-repl/bin/cua-repl.mjs"),
  (Row 12 10 'node_repl.exe' "$cua\v1\bin\node_repl.exe" "`"$cua\v1\bin\node_repl.exe`""),
  (Row 13 11 'node_repl.exe' "$cua\v1\bin\node_repl.exe" "`"$cua\v1\bin\node_repl.exe`""),
  (Row 14 10 'tunnel-client-runtime.exe' "$app\openai-tunnel\v1\tunnel-client-runtime.exe" 'tunnel-client-runtime.exe run --control-plane.tunnel-id fixture'),
  (Row 15 10 'conhost.exe' 'C:\Windows\System32\conhost.exe' 'conhost.exe'),
  (Row 20 10 'codexify-browser-companion.exe' "$app\browser\runtime\v1\app\codexify-browser-companion.exe" 'browser'),
  (Row 21 20 'chrome.exe' "$app\browser\runtime\v1\chrome.exe" 'chrome'),
  (Row 22 21 'powershell.exe' 'C:\Windows\powershell.exe' 'private renderer priority helper')
)
Check $base @() 'Known idle infrastructure'
Check ($base+@((Row 30 10 'powershell.exe' 'C:\Windows\powershell.exe' 'cargo build'),(Row 31 30 'cargo.exe' 'C:\cargo.exe' 'cargo build'),(Row 32 31 'rustc.exe' 'C:\rustc.exe' 'rustc'))) @(30,31,32) 'Yielded build remains busy'
Check ($base+@((Row 40 12 'node.exe' "$cua\v1\bin\node.exe" 'node project-server.mjs'))) @(40) 'User command through idle Node host'
Check ($base+@((Row 50 999 'cargo.exe' 'C:\cargo.exe' 'unrelated build'))) @() 'Unrelated project process'
Check @((Row 20 10 'codexify-browser-companion.exe' 'C:\project\other.exe' 'other'),(Row 21 20 'cargo.exe' 'C:\cargo.exe' 'build')) @(20,21) 'Unverified browser subtree is busy'
Check ($base+@((Row 60 10 'custom-build.exe' 'C:\project\custom-build.exe' 'custom'))) @(60) 'Unknown native child is busy'
$self=Get-CimInstance Win32_Process -Filter ('ProcessId='+$PID)
if(-not(Test-AgentUpdateProcessRunning $self)){throw 'Current test process must be detected as running'}
$fixture=Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList @('-NoProfile','-Command','exit 0') -WindowStyle Hidden -PassThru
try {
  $row=[pscustomobject]@{ProcessId=$fixture.Id;CreationDate=$fixture.StartTime}
  if(-not $fixture.WaitForExit(10000)){throw 'Fixture process did not finish'}
  if(Test-AgentUpdateProcessRunning $row){throw 'An exited process with a retained handle is not active work'}
} finally {$fixture.Dispose()}
if(-not(Test-AgentUpdateBrowserStopped ([pscustomobject]@{state='stopped';running=$false}) @())){throw 'Verified stopped status must be accepted'}
if(Test-AgentUpdateBrowserStopped ([pscustomobject]@{state='stopped';running=$false}) @([pscustomobject]@{ProcessId=999})){throw 'Stopped status must not hide a live private process'}
$idle=@{kind='codexify-browser-companion';status='idle';authenticated=$true;accountFingerprint='fixture';activeActionId=$null}
if(-not(Test-AgentUpdateBrowserIdle ([pscustomobject]$idle))){throw 'Verified idle legacy window must be accepted'}
foreach($change in @(@{status='busy'},@{authenticated=$false},@{activeActionId='accepted-send'},@{kind='other-service'})){
  $probe=$idle.Clone();foreach($key in $change.Keys){$probe[$key]=$change[$key]}
  if(Test-AgentUpdateBrowserIdle ([pscustomobject]$probe)){throw 'A busy, warming or unrelated window was accepted for update'}
}
$pool=$idle.Clone();$pool.activeActionIds=@('another-page-send');$pool.actionCapacity=4;$pool.availableActionSlots=3
if(Test-AgentUpdateBrowserIdle ([pscustomobject]$pool)){throw 'A free pool slot must not hide a Send owned by another page'}
$pool.activeActionIds=@()
if(Test-AgentUpdateBrowserIdle ([pscustomobject]$pool)){throw 'Unavailable pool capacity must defer updates even without a visible action ID'}
$pool.availableActionSlots=4
if(-not(Test-AgentUpdateBrowserIdle ([pscustomobject]$pool))){throw 'A completely idle verified pool must be accepted'}
$stopped=[pscustomobject]@{generationRunning=$false;observedActionPages=1}
if(-not(Test-AgentUpdateBrowserQuiescent ([pscustomobject]$idle) $stopped)){throw 'A verified idle single page with no generation must pass'}
foreach($diagnostics in @($null,[pscustomobject]@{},[pscustomobject]@{generationRunning=$true;observedActionPages=1})){
  if(Test-AgentUpdateBrowserQuiescent ([pscustomobject]$idle) $diagnostics){throw 'Running or unknown generation must defer the update'}
}
if(Test-AgentUpdateBrowserQuiescent ([pscustomobject]$pool) $stopped){throw 'Inspecting only one of four pages is insufficient'}
$allStopped=[pscustomobject]@{generationRunning=$false;observedActionPages=4}
if(-not(Test-AgentUpdateBrowserQuiescent ([pscustomobject]$pool) $allStopped)){throw 'All idle verified pages must pass'}

$tempRoot=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')+'\'
$fixtureRoot=Join-Path $tempRoot ('codexify-navigation-stop-test-'+[Guid]::NewGuid().ToString('N'))
$originalProfile=$env:USERPROFILE
try {
  $env:USERPROFILE=$fixtureRoot
  $fixtureScripts=Join-Path $fixtureRoot 'scripts'
  $fixtureArtifacts=Join-Path $fixtureRoot 'browser-companion/artifacts'
  $fixtureApp=Join-Path $fixtureRoot '.codexify'
  $browserRoot=Join-Path $fixtureApp 'browser'
  $resources=Join-Path $browserRoot 'runtime/2.3.15/app/resources'
  $null=New-Item -ItemType Directory -Path $fixtureScripts,$fixtureArtifacts,$resources,(Join-Path $fixtureApp 'backups') -Force
  foreach($name in @('windows-apply-navigation-stall-patch.ps1','windows-agent-update-guards.ps1')){
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $fixtureScripts
  }
  @'
$script:fixtureDiagnosticCalls=0
function Invoke-StartLocalCli { throw 'Unexpected native lifecycle command in stopped browser fixture' }
function Invoke-RestMethod {
  param($Uri,$Headers,$TimeoutSec)
  if($Uri -like '*/v1/health'){
    return [pscustomobject]@{kind='codexify-browser-companion';status='idle';authenticated=$true;accountFingerprint='fixture'}
  }
  if($Uri -notlike '*/v1/diagnostics/presentation'){throw 'Unexpected fixture request'}
  $script:fixtureDiagnosticCalls++
  if($script:fixtureDiagnosticCalls -eq 2){[IO.File]::WriteAllText($lifecyclePath,'{"automaticStart":false}')}
  [pscustomobject]@{generationRunning=$false;observedActionPages=1}
}
'@ | Set-Content -LiteralPath (Join-Path $fixtureScripts 'windows-start-local-core.ps1')
  $archive=Join-Path $resources 'app.asar'
  $candidate=Join-Path $fixtureArtifacts 'candidate.asar'
  [IO.File]::WriteAllText($archive,'fixture installed archive')
  [IO.File]::WriteAllText($candidate,'fixture candidate archive')
  $previousHash=(Get-FileHash -LiteralPath $archive).Hash
  @{version='2.3.15';live=$archive;candidate=$candidate;previousSha256=$previousHash;sha256=(Get-FileHash -LiteralPath $candidate).Hash} |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $fixtureArtifacts 'navigation-stall-ready.json')
  $lifecycle=Join-Path $browserRoot 'lifecycle.json'
  $fixtureUpdater=Join-Path $fixtureScripts 'windows-apply-navigation-stall-patch.ps1'
  [IO.File]::WriteAllText($lifecycle,'{"automaticStart":false}')
  $result=& $fixtureUpdater | ConvertFrom-Json
  if($result.reason -ne 'browser_explicitly_stopped'){throw 'Explicitly stopped browser with no descriptor must defer'}
  [IO.File]::WriteAllText($lifecycle,'{"automaticStart":true}')
  [IO.File]::WriteAllText((Join-Path $browserRoot 'descriptor.json'),'{"endpoint":"http://127.0.0.1:1","token":"fixture"}')
  [IO.File]::WriteAllText((Join-Path $fixtureApp 'codexify.config.json'),'{}')
  $result=& $fixtureUpdater | ConvertFrom-Json
  if($result.reason -ne 'browser_explicitly_stopped'){throw 'An explicit stop during updater preflight must defer'}
  if((Get-FileHash -LiteralPath $archive).Hash -ne $previousHash){throw 'Deferral changed the installed archive'}
} finally {
  $env:USERPROFILE=$originalProfile
  if(Test-Path -LiteralPath $fixtureRoot){
    $resolved=(Resolve-Path -LiteralPath $fixtureRoot).Path
    if(-not $resolved.StartsWith($tempRoot,[StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolved) -notlike 'codexify-navigation-stop-test-*'){throw 'Unsafe fixture cleanup path'}
    Remove-Item -LiteralPath $resolved -Recurse -Force
  }
}
'Passed 26 updater process, idle-window, generation and explicit-stop checks'
