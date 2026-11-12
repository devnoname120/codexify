#Requires -Version 7.0
param([switch]$Apply)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-start-local-core.ps1')
. (Join-Path $PSScriptRoot 'windows-agent-update-guards.ps1')
$repo=Split-Path -Parent $PSScriptRoot
$ready=Join-Path $repo 'browser-companion/artifacts/ready-20261030'
$receipt=Get-Content -LiteralPath (Join-Path $ready 'agent-update.json') -Raw|ConvertFrom-Json
$appRoot=Join-Path $env:USERPROFILE '.codexify'
$native=Join-Path $appRoot 'bin/codexify.exe'
$archive=Join-Path $appRoot ('browser/runtime/'+$receipt.version+'/app/resources/app.asar')
$activationPath=Join-Path $ready 'agent-activation.json'
function Defer([string]$Reason) { @{state='deferred';reason=$Reason}|ConvertTo-Json; exit 0 }
$buildStatePath=Join-Path $ready 'agent-build-state.json'
if(Test-Path -LiteralPath $buildStatePath){
  try {$buildState=Get-Content -LiteralPath $buildStatePath -Raw|ConvertFrom-Json}
  catch {Defer 'native_build_status_busy'}
  # The builder publishes the native file and matching receipt only after its
  # probe succeeds. Never race that publication while restarting the child.
  if($buildState.state -eq 'building'){Defer 'native_build_active'}
  if($buildState.state -eq 'failed'){Defer 'native_build_failed'}
}
if((Get-FileHash -LiteralPath $native -Algorithm SHA256).Hash -ine $receipt.nativeSha256){Defer 'native_binary_changed'}
$owner=Get-Content (Join-Path $appRoot 'owner-chat.json') -Raw|ConvertFrom-Json
$headers=@{Authorization=('Bearer '+$owner.token)}
$base='http://127.0.0.1:'+$owner.port
$settings=Invoke-RestMethod -NoProxy -Uri ($base+'/api/settings') -Headers $headers -TimeoutSec 8
$nativeActive=$false
# A feature flag proves the first upgrade, not every later build. Tie a
# verified activation to this exact binary and running server lifetime.
if($settings.defaults.agentChat.features.defaultDelegation -eq $true -and (Test-Path -LiteralPath $activationPath)){
  try {
    $activated=Get-Content -LiteralPath $activationPath -Raw|ConvertFrom-Json
    $running=Get-CimInstance Win32_Process -Filter ('ProcessId='+[uint32]$activated.serverPid)
    $nativeActive=$activated.nativeSha256 -ieq $receipt.nativeSha256 -and $running.ExecutablePath -ieq $native `
      -and ([DateTimeOffset]$running.CreationDate.ToUniversalTime()).ToUnixTimeMilliseconds() -eq $activated.serverStartedAtMs
  }catch{$nativeActive=$false}
}
$archiveHash=(Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
if($nativeActive -and $archiveHash -ieq $receipt.browserSha256){@{state='already_active'}|ConvertTo-Json; exit 0}
try {$chats=Invoke-RestMethod -NoProxy -Uri ($base+'/api/chats') -Headers $headers -TimeoutSec 15}
catch {Defer 'chat_list_busy'}
if(@($chats.chats|Where-Object goalStatus -EQ 'active').Count){Defer 'active_goal'}
$health=Invoke-RestMethod -NoProxy -Uri 'http://127.0.0.1:3000/health' -TimeoutSec 5
if($health.status -ne 'ok' -or $health.filesystem.occupiedWorkerSlots -gt 0 `
  -or @($health.upstreams.PSObject.Properties.Value|Where-Object activeRequests -GT 0).Count){Defer 'active_tools'}
$lifecycle=Get-Content (Join-Path $appRoot 'browser/lifecycle.json') -Raw|ConvertFrom-Json
if($lifecycle.automaticStart -isnot [bool]){Defer 'lifecycle_not_verified'}
$resumeBrowser=$lifecycle.automaticStart
$descriptorPath=Join-Path $appRoot 'browser/descriptor.json'
$descriptor=$null
if(Test-Path -LiteralPath $descriptorPath){
  $descriptor=Get-Content -LiteralPath $descriptorPath -Raw|ConvertFrom-Json
  try {
    $browser=Invoke-RestMethod -NoProxy -Uri ($descriptor.endpoint+'/v1/health') -Headers @{Authorization=('Bearer '+$descriptor.token)} -TimeoutSec 8
    if($browser.activeActionId){Defer 'active_browser_action'}
  } catch {
    $status=Invoke-StartLocalCli $native @('--config',(Join-Path $appRoot 'codexify.config.json'),'browser','status','--json')
    if(($status.Output|ConvertFrom-Json).state -ne 'stopped'){Defer 'browser_not_observable'}
  }
}
$serverId=@(Get-NetTCPConnection -LocalPort 3000 -State Listen|Select-Object -ExpandProperty OwningProcess -Unique)
if($serverId.Count -ne 1){Defer 'server_identity_not_verified'}
$server=Get-CimInstance Win32_Process -Filter ('ProcessId='+$serverId[0])
$parent=Get-CimInstance Win32_Process -Filter ('ProcessId='+$server.ParentProcessId)
if(-not $parent -or $server.ExecutablePath -ine $native -or $parent.ExecutablePath -ine $native `
  -or $parent.CommandLine -notmatch 'service\s+run'){Defer 'supervisor_identity_not_verified'}
$allProcesses=@(Get-CimInstance Win32_Process)
$privateBrowserId=if($descriptor){[uint32]$descriptor.pid}else{[uint32]0}
$work=@(Get-AgentUpdateWorkProcesses -Processes $allProcesses -ServerId $server.ProcessId -BrowserId $privateBrowserId `
  -AppRoot $appRoot -CuaRuntimeRoot (Join-Path $env:LOCALAPPDATA 'OpenAI/Codex/runtimes/cua_node') -SystemRoot $env:SystemRoot | Where-Object {Test-AgentUpdateProcessRunning $_})
if($work.Count){Defer 'resident_commands_active'}
$candidateName=if($receipt.PSObject.Properties['browserCandidate']){[string]$receipt.browserCandidate}else{'agent-browser.asar'}
if($candidateName -notmatch '^agent-browser(?:-[a-f0-9]{16,64})?\.asar$'){throw 'Invalid pinned browser candidate filename'}
$candidate=Join-Path $ready $candidateName
if((Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash -ine $receipt.browserSha256){throw 'Browser candidate receipt mismatch'}
if($archiveHash -ine $receipt.previousBrowserSha256 -and $archiveHash -ine $receipt.browserSha256){Defer 'browser_archive_changed'}
if(-not $Apply){@{state='idle_update_ready';nativeActive=$nativeActive;browserUpdateNeeded=($archiveHash -ine $receipt.browserSha256)}|ConvertTo-Json;exit 0}
# Recheck after inspection/hashing. Pausing a goal must never kill a yielded
# compiler or allow a newly started action to slip through the idle check.
$freshChats=Invoke-RestMethod -NoProxy -Uri ($base+'/api/chats') -Headers $headers -TimeoutSec 15
if(@($freshChats.chats|Where-Object goalStatus -EQ 'active').Count){Defer 'active_goal'}
$freshHealth=Invoke-RestMethod -NoProxy -Uri 'http://127.0.0.1:3000/health' -TimeoutSec 5
if($freshHealth.status -ne 'ok' -or $freshHealth.filesystem.occupiedWorkerSlots -gt 0 `
  -or @($freshHealth.upstreams.PSObject.Properties.Value|Where-Object activeRequests -GT 0).Count){Defer 'active_tools'}
$work=@(Get-AgentUpdateWorkProcesses -Processes @(Get-CimInstance Win32_Process) -ServerId $server.ProcessId -BrowserId $privateBrowserId `
  -AppRoot $appRoot -CuaRuntimeRoot (Join-Path $env:LOCALAPPDATA 'OpenAI/Codex/runtimes/cua_node') -SystemRoot $env:SystemRoot | Where-Object {Test-AgentUpdateProcessRunning $_})
if($work.Count){Defer 'resident_commands_active'}
# No work has been accepted on the browser. Preserve the existing supervisor,
# explicit browser-stop preference, configuration, profile and prior binaries.
$stop=Invoke-StartLocalCli $native @('--config',(Join-Path $appRoot 'codexify.config.json'),'browser','stop') -TimeoutMilliseconds 30000 -WorkingDirectory $appRoot
$prefix=[IO.Path]::GetFullPath((Join-Path $appRoot 'browser/runtime')).TrimEnd('\')+'\'
if($stop.ExitCode -ne 0){
  # The CLI can report its non-running status code after a successful stop.
  # Verify the desired state and real process exit instead of repeating a stop.
  $stoppedStatus=Invoke-StartLocalCli $native @('--config',(Join-Path $appRoot 'codexify.config.json'),'browser','status','--json') -TimeoutMilliseconds 15000 -WorkingDirectory $appRoot
  $livePrivate=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase) -and (Test-AgentUpdateProcessRunning $_)})
  if(-not(Test-AgentUpdateBrowserStopped -Status ($stoppedStatus.Output|ConvertFrom-Json) -LiveProcesses $livePrivate)){throw 'The private browser could not stop; application files were preserved'}
}
if(@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase) -and (Test-AgentUpdateProcessRunning $_)}).Count){throw 'Private browser processes are still exiting'}
$backupRoot=Join-Path $appRoot 'backups/agent-reliability'
$null=New-Item -ItemType Directory -Path $backupRoot -Force
if((Get-Item -LiteralPath $backupRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Application backup must be a regular directory'}
if($archiveHash -ine $receipt.browserSha256){
  $pending=$archive+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
  Copy-Item -LiteralPath $candidate -Destination $pending
  [IO.File]::Replace($pending,$archive,(Join-Path $backupRoot ('app-'+[Guid]::NewGuid().ToString('N')+'.asar')),$true)
}
if(-not $nativeActive){
  Stop-Process -Id $server.ProcessId -ErrorAction Stop
  $deadline=[DateTime]::UtcNow.AddSeconds(60)
  do {try {$health=Invoke-RestMethod -NoProxy -Uri 'http://127.0.0.1:3000/health' -TimeoutSec 2;if($health.status -eq 'ok'){break}}catch{};Start-Sleep -Milliseconds 400}while([DateTime]::UtcNow -lt $deadline)
  if($health.status -ne 'ok'){throw 'The existing supervisor has not restored server readiness'}
}
$owner=Get-Content (Join-Path $appRoot 'owner-chat.json') -Raw|ConvertFrom-Json
$headers=@{Authorization=('Bearer '+$owner.token)};$base='http://127.0.0.1:'+$owner.port
$settings=Invoke-RestMethod -NoProxy -Uri ($base+'/api/settings') -Headers $headers -TimeoutSec 8
if($settings.defaults.agentChat.features.defaultDelegation -ne $true){throw 'The running server did not expose the new defaults'}
if((Get-FileHash -LiteralPath $native -Algorithm SHA256).Hash -ine $receipt.nativeSha256){throw 'Native binary changed during activation'}
$runningId=@(Get-NetTCPConnection -LocalPort 3000 -State Listen|Select-Object -ExpandProperty OwningProcess -Unique)
if($runningId.Count -ne 1){throw 'Activated server identity could not be verified'}
$running=Get-CimInstance Win32_Process -Filter ('ProcessId='+$runningId[0])
if($running.ExecutablePath -ine $native -or $running.ParentProcessId -ne $parent.ProcessId){throw 'Activated server is not owned by the existing supervisor'}
$activated=@{nativeSha256=$receipt.nativeSha256;serverPid=$running.ProcessId;serverStartedAtMs=([DateTimeOffset]$running.CreationDate.ToUniversalTime()).ToUnixTimeMilliseconds()}
$temporary=$activationPath+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
[IO.File]::WriteAllText($temporary,($activated|ConvertTo-Json))
[IO.File]::Move($temporary,$activationPath,$true)
if($resumeBrowser){
  $null=Invoke-RestMethod -NoProxy -Uri ($base+'/api/new-chat/login') -Headers $headers -Method Post -ContentType 'application/json' -Body '{}' -TimeoutSec 45
  $start=Invoke-StartLocalCli $native @('--config',(Join-Path $appRoot 'codexify.config.json'),'browser','start') -TimeoutMilliseconds 20000 -WorkingDirectory $appRoot
  if($start.ExitCode -ne 0){throw 'Browser automatic control did not become ready'}
}
@{state='activated';nativeSha256=$receipt.nativeSha256;browserSha256=$receipt.browserSha256;browserResumed=$resumeBrowser;supervisorPid=$parent.ProcessId}|ConvertTo-Json
