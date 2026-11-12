#Requires -Version 7.0
param(
  [string]$Root=(Join-Path $env:USERPROFILE '.codexify'),
  [string]$ReceiptPath=(Join-Path $PSScriptRoot '../browser-companion/artifacts/navigation-stall-ready.json'),
  [string]$ActivationPath=(Join-Path $PSScriptRoot '../browser-companion/artifacts/navigation-stall-activation.json'),
  [switch]$Watch,
  [ValidateRange(1,180)][int]$MaxMinutes=90
)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-agent-update-guards.ps1')
$receipt=Get-Content -LiteralPath $ReceiptPath -Raw|ConvertFrom-Json
$browser=Join-Path $Root 'browser'
$archive=[IO.Path]::GetFullPath((Join-Path $browser ('runtime/'+$receipt.version+'/app/resources/app.asar')))
$prefix=[IO.Path]::GetFullPath((Join-Path $browser 'runtime')).TrimEnd('\')+'\'
$backupPath=$null
if($archive -ine [IO.Path]::GetFullPath($receipt.live)){throw 'Browser archive path differs from its receipt'}
if((Get-FileHash -LiteralPath $receipt.candidate).Hash -ine $receipt.sha256){throw 'Browser candidate checksum mismatch'}

function Save-Activation([string]$State) {
  $value=@{state=$State;pid=$PID;atUtc=[DateTime]::UtcNow.ToString('o');sha256=$receipt.sha256;archive=$archive;backup=$script:backupPath}
  if($State -eq 'already_staged' -and (Test-Path -LiteralPath $ActivationPath)){
    $previous=Get-Content -LiteralPath $ActivationPath -Raw|ConvertFrom-Json
    if($previous.sha256 -ieq $receipt.sha256 -and $previous.archive -ieq $archive){
      # Rechecking the same installation retains its rollback location.
      $value.backup=$previous.backup
    }
  }
  $temporary=$ActivationPath+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
  [IO.File]::WriteAllText($temporary,($value|ConvertTo-Json))
  [IO.File]::Move($temporary,[IO.Path]::GetFullPath($ActivationPath),$true)
  return [pscustomobject]$value
}
function Get-RunningBrowserProcesses {
  # Unknown executable identity also blocks staging. A live archive's lock is
  # an additional check, never a substitute for the managed process inventory.
  @(Get-CimInstance Win32_Process|Where-Object {
    ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase) -and
      (Test-AgentUpdateProcessRunning $_)) -or
    (-not $_.ExecutablePath -and $_.Name -eq 'codexify-browser-companion.exe')
  })
}
function Try-StageBrowserPatch {
  if((Get-FileHash -LiteralPath $archive).Hash -ieq $receipt.sha256){return 'already_staged'}
  if(@(Get-RunningBrowserProcesses).Count){return 'waiting_for_browser_exit'}
  $lifecycleLock=$null
  $locked=$false
  try {
    $lifecycleLock=[IO.File]::Open((Join-Path $browser 'lifecycle.lock'),[IO.FileMode]::Open,
      [IO.FileAccess]::ReadWrite,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {$lifecycleLock.Lock(0,1);$locked=$true}
    catch [IO.IOException] {return 'waiting_for_lifecycle_lock'}
    # Native recovery takes this same lock before starting a replacement. It
    # can win the race; observe again after acquiring it and wait for a later exit.
    if(@(Get-RunningBrowserProcesses).Count){return 'waiting_for_browser_exit'}
    $lifecycle=Get-Content -LiteralPath (Join-Path $browser 'lifecycle.json') -Raw|ConvertFrom-Json
    if($lifecycle.automaticStart -isnot [bool]){throw 'Browser start preference is unavailable'}
    if(-not $lifecycle.automaticStart){return 'browser_explicitly_stopped'}
    $currentHash=(Get-FileHash -LiteralPath $archive).Hash
    if($currentHash -ieq $receipt.sha256){return 'already_staged'}
    if($currentHash -ine $receipt.previousSha256){throw 'Browser archive changed since preparation'}
    if((Get-FileHash -LiteralPath $receipt.candidate).Hash -ine $receipt.sha256){throw 'Browser candidate changed before staging'}
    $backup=Join-Path $Root ('backups/browser-next-launch-'+[Guid]::NewGuid().ToString('N'))
    $null=New-Item -ItemType Directory -Path $backup
    $script:backupPath=Join-Path $backup 'app.asar'
    $pending=$archive+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
    Copy-Item -LiteralPath $receipt.candidate -Destination $pending
    [IO.File]::Replace($pending,$archive,$script:backupPath)
    if((Get-FileHash -LiteralPath $archive).Hash -ine $receipt.sha256){throw 'Staged browser checksum mismatch'}
    return 'staged_for_next_launch'
  } finally {
    if($locked){$lifecycleLock.Unlock(0,1)}
    if($lifecycleLock){$lifecycleLock.Dispose()}
  }
}

$watcher=$null
try {
  try {$watcher=[IO.File]::Open($ActivationPath+'.watch.lock',[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)}
  catch [IO.IOException] {@{state='watcher_already_running'}|ConvertTo-Json;return}
  $deadline=[DateTime]::UtcNow.AddMinutes($MaxMinutes)
  $state=Try-StageBrowserPatch
  Save-Activation $state|ConvertTo-Json
  while($Watch -and $state -in @('waiting_for_browser_exit','waiting_for_lifecycle_lock') -and [DateTime]::UtcNow -lt $deadline){
    # Waiting on the descriptor's process handle avoids polling the complete
    # machine process list while a healthy browser keeps working.
    $observed=$null
    try {
      $descriptor=Get-Content -LiteralPath (Join-Path $browser 'descriptor.json') -Raw|ConvertFrom-Json
      $observed=Get-Process -Id ([int]$descriptor.pid) -ErrorAction SilentlyContinue
      if($observed -and -not $observed.WaitForExit(1000)){continue}
    } catch {Start-Sleep -Milliseconds 250}
    finally {if($observed){$observed.Dispose()}}
    $next=Try-StageBrowserPatch
    if($next -ne $state){$state=$next;Save-Activation $state|ConvertTo-Json}
    if($state -in @('waiting_for_browser_exit','waiting_for_lifecycle_lock')){Start-Sleep -Milliseconds 250}
  }
  if($Watch -and $state -in @('waiting_for_browser_exit','waiting_for_lifecycle_lock')){Save-Activation 'watch_expired'|ConvertTo-Json}
} catch {
  Save-Activation 'failed'|Out-Null
  throw
} finally {if($watcher){$watcher.Dispose()}}
