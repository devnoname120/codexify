param(
  [Parameter(Mandatory=$true)][string]$Candidate,
  [Parameter(Mandatory=$true)][string]$Archive,
  [Parameter(Mandatory=$true)][string]$Backup,
  [switch]$Deploy,
  [switch]$AllowSameNative,
  [switch]$TuneAutowork,
  [switch]$FastSetup,
  [switch]$NativeOnly,
  [switch]$UseExistingTaskRecovery,
  [string[]]$ConfigSet=@(),
  # Interrupt running native commands instead of waiting for them. Agents rerun
  # interrupted commands after the restart; the browser must still be idle.
  [switch]$AllowActiveCommands,
  [string]$ExpectedAccountFingerprint
)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
# An enabled RestartCount/PT1M policy did not restart the task after a verified
# native failure. Never stop live work on that assumption again. Reject before
# profile shutdown, file changes, or any process/service management.
if($UseExistingTaskRecovery){
  throw 'UseExistingTaskRecovery is disabled: task failure recovery did not restart the service. Updates require permitted normal scheduled-task stop/start control; preserve the live service until manual maintenance is arranged.'
}
$root=Join-Path $env:USERPROFILE '.codexify'
$exe=Join-Path $root 'bin\codexify.exe'
$oldExe=Join-Path $Backup 'codexify.before-update.exe'
$server=$null
$stopped=$false
$browserQuiesced=$false
$replaced=$false
$counter=0
$previousReleaseRoot=$env:CODEXIFY_BROWSER_RELEASE_ROOT
. (Join-Path $PSScriptRoot 'windows-browser-graceful-update.ps1')
. (Join-Path $PSScriptRoot 'windows-browser-only-native-backup.ps1')
New-Item -ItemType Directory -Path $Backup -Force | Out-Null
function Record([string]$Stage,[string]$Detail) {
  @{stage=$Stage;detail=$Detail;atUtc=[DateTime]::UtcNow.ToString('o')} |
    ConvertTo-Json -Compress | Set-Content -LiteralPath (Join-Path $Backup 'update-status.json')
  Write-Output "$Stage`: $Detail"
}
function Invoke-Cli([string]$File,[string[]]$Arguments) {
  $script:counter++
  $out=Join-Path $Backup "cli-$counter.stdout"
  $err=Join-Path $Backup "cli-$counter.stderr"
  $process=Start-Process -FilePath $File -ArgumentList $Arguments -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput $out -RedirectStandardError $err
  # Windows PowerShell 5.1 reports a null ExitCode unless the handle is opened
  # before the process exits.
  $null=$process.Handle
  # Wait for the CLI itself, not its persistent browser/service descendants.
  if(-not $process.WaitForExit(180000)){throw "CLI timed out: $($Arguments -join ' ')"}
  $process.Refresh()
  if($process.ExitCode -ne 0){throw "CLI failed: $($Arguments -join ' '); see $err"}
  $stream=[IO.File]::Open($out,[IO.FileMode]::Open,[IO.FileAccess]::Read,
    ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
  $reader=[IO.StreamReader]::new($stream)
  try {return $reader.ReadToEnd()} finally {$reader.Dispose()}
}
function Browser-Idle {
  try {
    $descriptorPath=Join-Path $root 'browser\descriptor.json'
    if(-not(Test-Path -LiteralPath $descriptorPath)){return $true}
    $descriptor=Get-Content -LiteralPath $descriptorPath -Raw | ConvertFrom-Json
    # A dead descriptor owner cannot own an active browser Send. A replacement
    # launcher still starting has not published any control/action surface yet.
    if(-not(Get-CimInstance Win32_Process -Filter ("ProcessId="+[int]$descriptor.pid))){return $true}
    $endpoint=[uri]$descriptor.endpoint
    if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1'){return $false}
    $health=Invoke-RestMethod -Uri ($descriptor.endpoint.TrimEnd('/')+'/v1/health') -TimeoutSec 10 `
      -Headers @{Authorization=('Bearer '+$descriptor.token)}
    return $health.status -eq 'idle' -and $health.authenticated -and -not $health.activeActionId
  } catch { return $false }
}
function Active-Commands([int]$ServerId) {
  $processes=@(Get-CimInstance Win32_Process)
  $descendants=[Collections.Generic.HashSet[int]]::new()
  $null=$descendants.Add($ServerId)
  do {
    $changed=$false
    foreach($process in $processes){
      if($descendants.Contains([int]$process.ParentProcessId) -and $descendants.Add([int]$process.ProcessId)){$changed=$true}
    }
  } while($changed)
  return @($processes | Where-Object {$_.ProcessId -ne $ServerId -and $descendants.Contains([int]$_.ProcessId) `
    -and $_.Name -match '^(cargo|rustc|link|clang|cmake|make|ninja|wsl|python|cmd|powershell|pwsh)(\.exe)?$'})
}
function Wait-ManagedBrowserStopped {
  $runtime=[IO.Path]::GetFullPath((Join-Path $root 'browser\runtime')).TrimEnd('\')+'\'
  $deadline=[DateTime]::UtcNow.AddSeconds(30)
  do {
    $remaining=@(Get-CimInstance Win32_Process | Where-Object {$_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($runtime,[StringComparison]::OrdinalIgnoreCase)})
    if(-not $remaining.Count){Start-Sleep -Milliseconds 750;return}
    Start-Sleep -Seconds 1
  }while([DateTime]::UtcNow -lt $deadline)
  throw 'Private browser processes are still exiting; runtime files were preserved'
}
function Server-RequestsIdle {
  try {
    $health=Invoke-RestMethod 'http://127.0.0.1:3000/health' -TimeoutSec 3
    if($health.status -ne 'ok' -or $health.filesystem.occupiedWorkerSlots -gt 0 -or $health.filesystem.queuedRequests -gt 0){return $false}
    foreach($upstream in $health.upstreams.PSObject.Properties){
      if($upstream.Value.activeRequests -gt 0 -or $upstream.Value.queuedRequests -gt 0){return $false}
    }
    return $true
  }catch{return $false}
}
function Restore-NativePriority {
  # The native launcher now starts the server at Normal even when an existing
  # task definition is locked. Normalize the older wrapper processes too.
  Get-CimInstance Win32_Process -Filter "Name='codexify.exe'" |
    Where-Object {$_.ExecutablePath -ieq $exe} |
    ForEach-Object {
      $targetProcess=Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue
      if($targetProcess){
        try {$targetProcess.PriorityClass='Normal'}
        catch {if(-not $targetProcess.HasExited){throw}}
      }
    }
}
function Confirm-ExistingTaskRecovery([object]$Listener) {
  $tasks=@(Get-ScheduledTask -TaskName 'Codexify' -TaskPath '\' -ErrorAction Stop)
  if($tasks.Count -ne 1){throw 'Expected the existing root Codexify scheduled task'}
  $task=$tasks[0]
  if($task.TaskName -cne 'Codexify' -or $task.TaskPath -ne '\' -or -not $task.Settings.Enabled `
    -or [int]$task.State -ne 4 -or [int]$task.Settings.RestartCount -lt 1 `
    -or $task.Settings.RestartInterval -cne 'PT1M'){
    throw 'Existing Codexify task is not running with enabled one-minute failure recovery; live service preserved'
  }
  $currentIdentity=[Security.Principal.WindowsIdentity]::GetCurrent()
  $principal=[string]$task.Principal.UserId
  if($principal -match '^S-1-'){$principalSid=[Security.Principal.SecurityIdentifier]::new($principal)}
  else {$principalSid=[Security.Principal.NTAccount]::new($principal).Translate([Security.Principal.SecurityIdentifier])}
  if($principalSid.Value -ne $currentIdentity.User.Value -or [int]$task.Principal.RunLevel -ne 0 `
    -or [int]$task.Principal.LogonType -ne 3){throw 'Existing Codexify task must run interactively as this user without elevation'}
  $actions=@($task.Actions)
  if($actions.Count -ne 1){throw 'Existing Codexify task must have exactly one verified action'}
  $action=$actions[0]
  $engine=[IO.Path]::GetFullPath([string]$action.Execute)
  $trustedEngines=@((Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'))
  if($env:ProgramFiles){$trustedEngines+=Join-Path $env:ProgramFiles 'PowerShell\7\pwsh.exe'}
  if($engine -notin $trustedEngines -or -not(Test-Path -LiteralPath $engine -PathType Leaf) `
    -or [IO.Path]::GetFullPath([string]$action.WorkingDirectory) -ine [IO.Path]::GetFullPath($root)){
    throw 'Existing Codexify task action or working directory is not the verified user installation'
  }
  $arguments=[string]$action.Arguments
  if($arguments -notmatch '(?i)^\s*-NoLogo\s+-NoProfile\s+-NonInteractive\s+-ExecutionPolicy\s+Bypass\s+-WindowStyle\s+Hidden\s+-EncodedCommand\s+([A-Za-z0-9+/]+={0,2})\s*$'){
    throw 'Existing Codexify task does not use the expected hidden encoded PowerShell wrapper'
  }
  $encoded=$matches[1]
  $bytes=[Convert]::FromBase64String($encoded)
  if($bytes.Length -gt 16384 -or $bytes.Length % 2 -ne 0){throw 'Existing task wrapper encoding is invalid'}
  $wrapper=[Text.Encoding]::Unicode.GetString($bytes).Replace("`r`n","`n").Trim()
  $quotedExe="'"+$exe.Replace("'","''")+"'"
  $quotedConfig="'"+(Join-Path $root 'codexify.config.json').Replace("'","''")+"'"
  $expected='$ErrorActionPreference = ''Stop'''+"`n& $quotedExe service run --config $quotedConfig`n"+'if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }'
  if($wrapper -cne $expected){throw 'Existing task wrapper must invoke the exact native/config paths and propagate native failure'}
  $supervisor=Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$Listener.ParentProcessId)
  $wrapperProcess=if($supervisor){Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$supervisor.ParentProcessId)}
  if(-not $supervisor -or $supervisor.ExecutablePath -ine $exe -or -not $wrapperProcess `
    -or $wrapperProcess.ExecutablePath -ine $engine -or -not $wrapperProcess.CommandLine.Contains($encoded)){
    throw 'Live native listener ancestry does not match the verified existing task wrapper'
  }
  return $task
}
function Stop-VerifiedNativePair([object]$Listener) {
  # Only the native supervisor and its verified listener are owned here. The
  # unchanged PowerShell task wrapper propagates their failure to its recovery.
  foreach($nativeId in @($Listener.ParentProcessId,$Listener.ProcessId)){
    $nativeProcess=Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$nativeId)
    if($nativeProcess -and $nativeProcess.ExecutablePath -ieq $exe){Stop-Process -Id $nativeId -Force}
  }
}
function Get-VerifiedNativeListener {
  $owners=@(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object {$_.LocalPort -in @(3000,3120)} | Select-Object -ExpandProperty OwningProcess -Unique)
  if(-not $owners.Count){return $null}
  if($owners.Count -ne 1){throw 'Codexify listeners no longer share one verified native process'}
  $listener=Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$owners[0])
  if(-not $listener -or $listener.ExecutablePath -ine $exe){throw 'Unexpected native listener during existing task recovery'}
  return $listener
}
function Wait-NativeHealthy([int]$Seconds) {
  $deadline=[DateTime]::UtcNow.AddSeconds($Seconds)
  do {
    try {
      if((Invoke-RestMethod 'http://127.0.0.1:3000/health' -TimeoutSec 3).status -eq 'ok'){
        $listener=Get-VerifiedNativeListener
        if($listener){return $true}
      }
    }catch{}
    Start-Sleep -Seconds 1
  }while([DateTime]::UtcNow -lt $deadline)
  return $false
}
function Wait-BrowserReady([string]$Version) {
  $deadline=[DateTime]::UtcNow.AddSeconds(120)
  do {
    try {
      $descriptor=Get-Content -LiteralPath (Join-Path $root 'browser\descriptor.json') -Raw | ConvertFrom-Json
      $endpoint=[uri]$descriptor.endpoint
      if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1'){throw 'Unexpected browser endpoint'}
      $health=Invoke-RestMethod ($descriptor.endpoint.TrimEnd('/')+'/v1/health') -TimeoutSec 5 -Headers @{Authorization=('Bearer '+$descriptor.token)}
      if($health.authenticated -and $health.status -in @('idle','busy') -and $health.build.StartsWith($Version+'+protocol.') -and (-not $ExpectedAccountFingerprint -or $health.accountFingerprint -eq $ExpectedAccountFingerprint)){return $true}
    } catch {}
    Start-Sleep -Seconds 1
  } while([DateTime]::UtcNow -lt $deadline)
  return $false
}
try {
  foreach($path in @($Candidate,$Archive,$exe)){if(-not(Test-Path -LiteralPath $path -PathType Leaf)){throw "Missing artifact: $path"}}
  if(Test-Path -LiteralPath $oldExe){throw 'Backup slot already used; choose a fresh backup directory'}
  $candidateHash=(Get-FileHash -LiteralPath $Candidate -Algorithm SHA256).Hash
  $oldHash=(Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
  if($candidateHash -eq $oldHash -and -not $AllowSameNative){throw 'Candidate equals the installed binary; use AllowSameNative only for a verified browser-only patch'}
  if($candidateHash -ne $oldHash -and (Test-Path -LiteralPath (Join-Path $root 'bin\codexify.incoming-local.exe'))){throw 'Binary staging path already exists; resolve it before stopping the live service'}
  $null=Invoke-Cli $Candidate @('--help')
  $listeners=@(Get-NetTCPConnection -State Listen | Where-Object {$_.LocalPort -in @(3000,3120)} | Select-Object -ExpandProperty OwningProcess -Unique)
  if($listeners.Count -ne 1){throw 'Expected one server owning the two Codexify listeners'}
  $live=Get-CimInstance Win32_Process -Filter "ProcessId=$($listeners[0])"
  if($live.ExecutablePath -ine $exe){throw 'Unexpected listener executable'}
  if($UseExistingTaskRecovery){
    $null=Confirm-ExistingTaskRecovery $live
    Record 'existing_task_recovery_verified' 'Existing user task, encoded native wrapper and one-minute failure recovery verified; task definition will remain unchanged'
  }
  Record 'preflight_verified' 'Candidate and live listener verified; current chats retained'
  if(-not $Deploy){return}
  # Backup directories may retain prior attempt records. Read only this new
  # file-server invocation's readiness receipt; keep the old receipts intact.
  $readyFile=Join-Path $Backup ('release-server-'+[guid]::NewGuid().ToString('N')+'.json')
  $serverScript=Join-Path $PSScriptRoot 'windows-update-file-server.cjs'
  $node=(Get-Command node.exe -ErrorAction Stop).Source
  $server=Start-Process -FilePath $node -ArgumentList @(('"'+$serverScript+'"'),('"'+$Archive+'"'),('"'+$readyFile+'"')) -WindowStyle Hidden -PassThru
  for($i=0;$i -lt 60 -and -not(Test-Path -LiteralPath $readyFile);$i++){Start-Sleep -Milliseconds 250}
  if(-not(Test-Path -LiteralPath $readyFile)){throw 'Local release server did not start'}
  $release=Get-Content -LiteralPath $readyFile -Raw | ConvertFrom-Json
  $env:CODEXIFY_BROWSER_RELEASE_ROOT=$release.root
  Record 'waiting_for_idle' 'Waiting for an idle browser and completion of active native commands'
  $ready=$false
  for($i=0;$i -lt 300;$i++){
    if((Browser-Idle) -and ($AllowActiveCommands -or ((Active-Commands $live.ProcessId).Count -eq 0 -and (Server-RequestsIdle)))){$ready=$true;break}
    Start-Sleep -Seconds 2
  }
  if(-not $ready){throw 'Active work did not settle; live service was not interrupted'}
  Record 'closing_browser_cleanly' 'Closing the verified private Chrome profile normally before stopping its Windows job'
  $runtimeRoot=[IO.Path]::GetFullPath((Join-Path $root 'browser\runtime')).TrimEnd('\')+'\'
  $managedBrowser=@(Get-CimInstance Win32_Process | Where-Object {$_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($runtimeRoot,[StringComparison]::OrdinalIgnoreCase)})
  if(-not $managedBrowser.Count){
    # Already stopped: no Chrome owns the profile, so back it up directly.
    $profileCopy=Join-Path $Backup 'browser-profile'
    Copy-Item -LiteralPath (Join-Path $root 'browser\chrome-profile') -Destination $profileCopy -Recurse
    $receipt=@{graceful=$true;profileBackupVerified=(Test-Path -LiteralPath (Join-Path $profileCopy 'Local State'))}
  } else {
  $receipt=Stop-ManagedBrowserGracefully -BrowserRoot (Join-Path $root 'browser') -NativeExe $exe -BackupDirectory $Backup -ExpectedAccountFingerprint $ExpectedAccountFingerprint
  }
  if(-not $receipt.graceful -or -not $receipt.profileBackupVerified){throw 'Clean browser shutdown was not verified'}
  $browserQuiesced=$true
  if($UseExistingTaskRecovery -and (Get-Content -LiteralPath (Join-Path $root 'browser\lifecycle.json') -Raw | ConvertFrom-Json).automaticStart -ne $false){throw 'Browser autostart was not disabled; existing task recovery update deferred'}
  Record 'browser_profile_saved' 'Chrome closed cleanly; encrypted cookies, Local State and profile files backed up and verified'
  if(-not $AllowActiveCommands -and ((Active-Commands $live.ProcessId).Count -ne 0 -or -not (Server-RequestsIdle))){throw 'Native work became active during the profile snapshot; service replacement was deferred'}
  if($UseExistingTaskRecovery){$null=Confirm-ExistingTaskRecovery $live}
  $stopped=$true
  if(-not $UseExistingTaskRecovery){$null=Invoke-Cli $exe @('service','stop')}
  # Task Scheduler can leave an old wrapper detached. Stop only the two
  # verified live processes; check identity again to avoid PID reuse.
  Stop-VerifiedNativePair $live
  $nativeExitDeadline=[DateTime]::UtcNow.AddSeconds(15)
  do {
    $remainingNative=@(Get-CimInstance Win32_Process -Filter "Name='codexify.exe'" | Where-Object {$_.ExecutablePath -ieq $exe})
    if(-not $remainingNative.Count){break}
    Start-Sleep -Milliseconds 500
  }while([DateTime]::UtcNow -lt $nativeExitDeadline)
  if($remainingNative.Count){throw ('An old Codexify process is still running: '+(($remainingNative | Select-Object -ExpandProperty ProcessId) -join ','))}
  # Chrome has already saved its profile and native autostart is disabled.
  # The watchdog is now stopped before replacing any browser runtime files.
  try {$null=Invoke-Cli $exe @('browser','stop')} catch {Wait-ManagedBrowserStopped}
  Wait-ManagedBrowserStopped
  Record 'snapshot' 'Saving configuration and durable chat state before replacement'
  foreach($name in @('codexify.config.json','owner-chat.json','projects','conversation-continuations','conversation-projects')){
    $source=Join-Path $root $name
    if(Test-Path -LiteralPath $source -PathType Leaf){Copy-Item -LiteralPath $source -Destination (Join-Path $Backup $name)}
    elseif(Test-Path -LiteralPath $source){
      # Chat state can exceed MAX_PATH; robocopy handles long paths natively.
      $null=& robocopy.exe $source (Join-Path $Backup $name) /E /R:1 /W:1 /NFL /NDL /NJH /NJS /NP
      if($LASTEXITCODE -ge 8){throw "Snapshot copy failed for $name (robocopy $LASTEXITCODE)"}
    }
  }
  if($candidateHash -eq $oldHash){
    $null=Backup-NativeForBrowserOnlyUpdate -Executable $exe -Candidate $Candidate -BackupExecutable $oldExe
    Record 'native_preserved' 'Browser-only update: native executable backed up and verified without staging or replacing it'
  } else {
    $stage=Join-Path $root 'bin\codexify.incoming-local.exe'
    if(Test-Path -LiteralPath $stage){throw 'Binary staging path already exists'}
    Copy-Item -LiteralPath $Candidate -Destination $stage
    if((Get-FileHash -LiteralPath $stage -Algorithm SHA256).Hash -ne $candidateHash){throw 'Staged binary hash mismatch'}
    Move-Item -LiteralPath $exe -Destination $oldExe
    Move-Item -LiteralPath $stage -Destination $exe
    $replaced=$true
  }
  if($TuneAutowork){
    $configPath=Join-Path $root 'codexify.config.json'
    $config=Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json -AsHashtable
    $config.agentChat.automation.automaticRetries=$true
    $config.agentChat.automation.retryTimings=@{
      infiniteRetries=$true
      actionDelaysSeconds=@(10,30,60,120)
      chatgptAccessDelaysSeconds=@(30,60,120,300)
      goalRecoveryDelaysSeconds=@(30,60,120)
      conversationLoadIntervalSeconds=30
      conversationUnavailableCooldownSeconds=60
      rateLimitCooldownSeconds=180
      approvalResumeGraceSeconds=180
      jitterPercent=10
    }
    $configStage=Join-Path $root 'codexify.config.incoming-autowork.json'
    [IO.File]::WriteAllText($configStage,($config | ConvertTo-Json -Depth 100),[Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $configStage -Destination $configPath -Force
    $null=Invoke-Cli $exe @('config','validate')
    Record 'autowork_tuned' 'Infinite retries enabled; bounded backoff and 30-second shared conversation load interval'
  }
  if($FastSetup){
    $null=Invoke-Cli $exe @('--config',(Join-Path $root 'codexify.config.json'),'config','fast-setup')
    Record 'fast_setup_applied' 'Validated fast setup applied; model choices and renewal limits preserved'
  }
  if($ConfigSet.Count){
    # Apply validated settings between replacement and restart, so the updated
    # server and browser start once with the final configuration.
    foreach($entry in $ConfigSet){
      $separator=$entry.IndexOf('=')
      if($separator -lt 1){throw "ConfigSet entries must be key=value: $entry"}
      $null=Invoke-Cli $exe @('--config',(Join-Path $root 'codexify.config.json'),'config','set',$entry.Substring(0,$separator),$entry.Substring($separator+1))
    }
    $null=Invoke-Cli $exe @('--config',(Join-Path $root 'codexify.config.json'),'config','validate')
    Record 'config_set_applied' ('Applied validated settings: '+($ConfigSet -join ', '))
  }
  if($NativeOnly){
    Record 'browser_runtime_preserved' 'Native-only update: keeping the existing installed browser runtime'
  } else {
    Record 'installing_browser' 'Installing checksum-verified local browser package through the native installer'
    $null=Invoke-Cli $exe @('browser','install')
  }
  if($UseExistingTaskRecovery){
    # Installation may exceed the task's existing one-minute restart interval.
    # Refresh only an idle verified server, so startup reads the final settings.
    $recovered=Get-VerifiedNativeListener
    if(-not $recovered -and @(Get-CimInstance Win32_Process -Filter "Name='codexify.exe'" | Where-Object {$_.ExecutablePath -ieq $exe}).Count){
      Record 'installed_waiting_for_idle_restart' 'Native recovery is starting without verified listeners; preserving its processes and deferring settings refresh'
      return
    }
    if($recovered){
      if((Active-Commands $recovered.ProcessId).Count -ne 0 -or -not (Server-RequestsIdle) -or -not (Browser-Idle)){
        Record 'installed_waiting_for_idle_restart' 'Existing task recovered during installation and native work is active; preserving it and deferring settings refresh'
        return
      }
      if((Get-Content -LiteralPath (Join-Path $root 'browser\lifecycle.json') -Raw | ConvertFrom-Json).automaticStart -ne $false){
        Record 'installed_waiting_for_idle_restart' 'Browser lifecycle changed during installation; preserving the live service and deferring settings refresh'
        return
      }
      $null=Confirm-ExistingTaskRecovery $recovered
      Stop-VerifiedNativePair $recovered
      $exitDeadline=[DateTime]::UtcNow.AddSeconds(15)
      while((Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$recovered.ProcessId)) -and [DateTime]::UtcNow -lt $exitDeadline){Start-Sleep -Milliseconds 250}
      if(Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$recovered.ProcessId)){throw 'Recovered native server did not exit; configuration refresh remains pending'}
    }
    Record 'waiting_for_existing_task_recovery' 'Waiting up to 120 seconds for the unchanged scheduled task to recover the updated native service'
  } else {
    try {$null=Invoke-Cli $exe @('service','start')}
    catch {
      # A newer binary upgrades the task launcher on start. An elevated task
      # cannot be modified without admin rights; it can still be started, and
      # its unchanged action runs the replaced executable.
      $startError=Get-Content -LiteralPath (Join-Path $Backup "cli-$counter.stderr") -Raw
      if($startError -notmatch 'Access is denied'){throw}
      Record 'task_launcher_preserved' 'Scheduled task launcher could not be upgraded without admin rights; starting the existing task'
      Start-ScheduledTask -TaskName 'Codexify'
    }
  }
  $healthy=Wait-NativeHealthy $(if($UseExistingTaskRecovery){120}else{60})
  if(-not $healthy){throw 'Updated server never became healthy'}
  $newServerId=Get-NetTCPConnection -State Listen -LocalPort 3000 | Select-Object -First 1 -ExpandProperty OwningProcess
  $initialPriority=(Get-Process -Id $newServerId).PriorityClass
  Record 'native_server_priority' ("Server started at $initialPriority priority before maintenance normalization")
  # Task Scheduler can publish its process after Start-ScheduledTask returns.
  # Apply priority only once the new native listener actually exists.
  Restore-NativePriority
  $null=Invoke-Cli $exe @('browser','start')
  if(-not (Wait-BrowserReady $release.version)){
    Record 'installed_waiting_for_browser_account' 'Native server and installed hashes verified; keeping the current browser alive while account verification settles'
    return
  }
  Record 'completed' ('Installed local '+$release.version+' update; server and browser healthy; rollback binary retained')
} catch {
  $failure=$_.Exception.Message
  @{message=$failure;position=$_.InvocationInfo.PositionMessage;stack=$_.ScriptStackTrace;atUtc=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Backup 'failure-detail.json')
  Record 'failed' $failure
  if($stopped){
    try {
      if($replaced){
        if($UseExistingTaskRecovery){
          $rollbackLive=Get-VerifiedNativeListener
          if($rollbackLive){
            if((Active-Commands $rollbackLive.ProcessId).Count -ne 0 -or -not (Server-RequestsIdle) -or -not (Browser-Idle)){throw 'Rollback deferred because the recovered native service is working; its processes and files were preserved'}
            $null=Confirm-ExistingTaskRecovery $rollbackLive
            try {$null=Invoke-Cli $exe @('browser','stop')} catch {}
            Stop-VerifiedNativePair $rollbackLive
          } elseif(@(Get-CimInstance Win32_Process -Filter "Name='codexify.exe'" | Where-Object {$_.ExecutablePath -ieq $exe}).Count){
            throw 'Rollback deferred while a native process is starting without verified listeners; live files preserved'
          } elseif(-not (Browser-Idle)){
            throw 'Rollback deferred because the browser is active without verified native listeners; live browser preserved'
          } else {
            try {$null=Invoke-Cli $exe @('browser','stop')} catch {}
          }
        } else {
          try {$null=Invoke-Cli $exe @('browser','stop')} catch {}
          try {$null=Invoke-Cli $exe @('service','stop')} catch {}
          Get-CimInstance Win32_Process -Filter "Name='codexify.exe'" | Where-Object {$_.ExecutablePath -ieq $exe} | ForEach-Object {Stop-Process -Id $_.ProcessId -Force}
        }
        Start-Sleep -Seconds 1
        Move-Item -LiteralPath $exe -Destination (Join-Path $Backup 'codexify.failed-update.exe')
        Move-Item -LiteralPath $oldExe -Destination $exe
      } elseif(-not(Test-Path -LiteralPath $exe) -and (Test-Path -LiteralPath $oldExe)) {Move-Item -LiteralPath $oldExe -Destination $exe}
      if(($TuneAutowork -or $FastSetup -or $ConfigSet.Count) -and (Test-Path -LiteralPath (Join-Path $Backup 'codexify.config.json'))){
        Copy-Item -LiteralPath (Join-Path $Backup 'codexify.config.json') -Destination (Join-Path $root 'codexify.config.json') -Force
      }
      if($UseExistingTaskRecovery){
        Record 'waiting_for_existing_task_rollback' 'Waiting for the unchanged task to recover the restored native binary and configuration'
        if(-not (Wait-NativeHealthy 120)){throw 'Restored native service did not become healthy through existing task recovery'}
      } else {$null=Invoke-Cli $exe @('service','start')}
      $null=Invoke-Cli $exe @('browser','start')
      Restore-NativePriority
      Record 'rolled_back' 'Previous binary restored and service/browser restarted; chat state preserved'
    } catch {Record 'rollback_failed' $_.Exception.Message}
  } elseif($browserQuiesced){
    try {$null=Invoke-Cli $exe @('browser','start');Record 'deferred' 'Native service preserved and cleanly closed browser reopened after deferred update'} catch {Record 'browser_resume_pending' $_.Exception.Message}
  }
  throw $failure
} finally {
  $env:CODEXIFY_BROWSER_RELEASE_ROOT=$previousReleaseRoot
  if($server -and -not $server.HasExited){Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue}
}
