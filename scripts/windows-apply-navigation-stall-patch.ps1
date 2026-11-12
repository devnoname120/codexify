# Apply a pinned browser-only repair; leave the native server and goals running.
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
. (Join-Path $PSScriptRoot 'windows-start-local-core.ps1')
. (Join-Path $PSScriptRoot 'windows-agent-update-guards.ps1')
$repo=Split-Path -Parent $PSScriptRoot
$receipt=Get-Content -LiteralPath (Join-Path $repo 'browser-companion/artifacts/navigation-stall-ready.json') -Raw|ConvertFrom-Json
$root=Join-Path $env:USERPROFILE '.codexify'
$native=Join-Path $root 'bin/codexify.exe'
$browser=Join-Path $root 'browser'
$lifecyclePath=Join-Path $browser 'lifecycle.json'
function Test-BrowserAutomaticStart {
  # Match the native default when no lifecycle preference has been saved.
  if(-not(Test-Path -LiteralPath $lifecyclePath)){return $true}
  $lifecycle=Get-Content -LiteralPath $lifecyclePath -Raw|ConvertFrom-Json
  if($lifecycle.automaticStart -isnot [bool]){throw 'Browser start preference could not be verified'}
  return $lifecycle.automaticStart
}
if(-not(Test-BrowserAutomaticStart)){
  @{state='deferred';reason='browser_explicitly_stopped'}|ConvertTo-Json
  return
}
$archive=Join-Path $browser ('runtime/'+$receipt.version+'/app/resources/app.asar')
if($archive -ine $receipt.live){throw 'Unexpected live browser archive'}
if((Get-FileHash -LiteralPath $archive).Hash -ine $receipt.previousSha256){throw 'Browser archive changed since patch preparation'}
if((Get-FileHash -LiteralPath $receipt.candidate).Hash -ine $receipt.sha256){throw 'Candidate checksum mismatch'}
$descriptor=Get-Content -LiteralPath (Join-Path $browser 'descriptor.json') -Raw|ConvertFrom-Json
$endpoint=[uri]$descriptor.endpoint
if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1'){throw 'Unexpected control endpoint'}
$headers=@{Authorization=('Bearer '+$descriptor.token)}
$deadline=[datetime]::UtcNow.AddSeconds(45)
$verifiedStopped=$false
do {
  try {$health=Invoke-RestMethod ($descriptor.endpoint+'/v1/health') -Headers $headers -TimeoutSec 5; $verifiedStopped=$false}
  catch {
    $ownedPrefix=[IO.Path]::GetFullPath((Join-Path $browser 'runtime')).TrimEnd('\')+'\'
    $running=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($ownedPrefix,[StringComparison]::OrdinalIgnoreCase) -and (Test-AgentUpdateProcessRunning $_)})
    if($running.Count){throw}
    $health=$null; $verifiedStopped=$true
  }
  if($verifiedStopped -or (Test-AgentUpdateBrowserIdle $health)){break}
  # Accepted sends and unsent navigation both keep ownership until they settle.
  Start-Sleep -Milliseconds 350
}while([datetime]::UtcNow -lt $deadline)
if(-not $verifiedStopped -and -not (Test-AgentUpdateBrowserIdle $health)){throw 'Browser has not verified an idle window; update deferred'}
if(-not $verifiedStopped){
  $diagnostics=Invoke-RestMethod ($descriptor.endpoint+'/v1/diagnostics/presentation') -Headers $headers -TimeoutSec 8
  if(-not(Test-AgentUpdateBrowserQuiescent $health $diagnostics)){throw 'Browser generation has not been verified stopped on every page; update deferred'}
}
$backup=Join-Path $root ('backups/navigation-stall-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $backup
Copy-Item -LiteralPath $archive -Destination (Join-Path $backup 'app.asar')
Copy-Item -LiteralPath (Join-Path $root 'codexify.config.json') -Destination (Join-Path $backup 'codexify.config.json')
# Backup I/O can outlast the idle gap. Recheck ownership immediately before Stop.
if(Test-Path -LiteralPath (Join-Path $browser 'descriptor.json')){
  $health=Invoke-RestMethod ($descriptor.endpoint+'/v1/health') -Headers $headers -TimeoutSec 5
  if(-not (Test-AgentUpdateBrowserIdle $health)){throw 'The browser is no longer verified idle after backup; update deferred'}
  $diagnostics=Invoke-RestMethod ($descriptor.endpoint+'/v1/diagnostics/presentation') -Headers $headers -TimeoutSec 8
  if(-not(Test-AgentUpdateBrowserQuiescent $health $diagnostics)){throw 'Browser generation changed or is unverified after backup; update deferred'}
}
if(-not(Test-BrowserAutomaticStart)){
  @{state='deferred';reason='browser_explicitly_stopped'}|ConvertTo-Json
  return
}
try {
$stop=Invoke-StartLocalCli $native @('--config',(Join-Path $root 'codexify.config.json'),'browser','stop') -TimeoutMilliseconds 45000 -WorkingDirectory $root
$prefix=[IO.Path]::GetFullPath((Join-Path $browser 'runtime')).TrimEnd('\')+'\'
$stoppingDeadline=[datetime]::UtcNow.AddSeconds(15)
do {
  $remaining=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase) -and (Test-AgentUpdateProcessRunning $_)})
  if($remaining.Count){Start-Sleep -Milliseconds 250}
}while($remaining.Count -and [datetime]::UtcNow -lt $stoppingDeadline)
if($remaining.Count){throw 'Managed browser is still exiting; runtime preserved'}
foreach($relative in @('Local State','Default/Preferences','Default/Network/Cookies')){
  $source=Join-Path (Join-Path $browser 'chrome-profile') $relative
  if(Test-Path -LiteralPath $source){
    $target=Join-Path $backup ('profile/'+$relative)
    $null=New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force
    try {Copy-Item -LiteralPath $source -Destination $target}
    catch [IO.IOException] {Write-Warning ('Optional profile snapshot is locked; existing profile is preserved: '+$relative)}
  }
}
$pending=$archive+'.'+[guid]::NewGuid().ToString('N')+'.tmp'
Copy-Item -LiteralPath $receipt.candidate -Destination $pending
[IO.File]::Replace($pending,$archive,(Join-Path $backup 'replaced-app.asar'))
if((Get-FileHash -LiteralPath $archive).Hash -ine $receipt.sha256){throw 'Installed browser checksum mismatch'}
} finally {
$start=Invoke-StartLocalCli $native @('--config',(Join-Path $root 'codexify.config.json'),'browser','start') -TimeoutMilliseconds 45000 -WorkingDirectory $root
}
if($start.ExitCode -ne 0){
  # The CLI observation budget can expire before a healthy launcher publishes
  # its descriptor. Verify the existing launch rather than starting it again.
  $ready=$false;$readyDeadline=[datetime]::UtcNow.AddSeconds(30)
  do {
    try {
      $fresh=Get-Content -LiteralPath (Join-Path $browser 'descriptor.json') -Raw|ConvertFrom-Json
      $freshEndpoint=[uri]$fresh.endpoint
      if($freshEndpoint.Scheme -ne 'http' -or $freshEndpoint.Host -ne '127.0.0.1'){throw 'Unexpected restarted endpoint'}
      $check=Invoke-RestMethod ($fresh.endpoint+'/v1/health') -Headers @{Authorization=('Bearer '+$fresh.token)} -TimeoutSec 3
      $ready=$check.kind -eq 'codexify-browser-companion'
    }catch{}
    if(-not $ready){Start-Sleep -Milliseconds 500}
  }while(-not $ready -and [datetime]::UtcNow -lt $readyDeadline)
  if(-not $ready){throw ('Browser restart failed: '+$start.ErrorOutput)}
}
@{state='installed';backup=$backup;sha256=$receipt.sha256;stopExit=$stop.ExitCode;startExit=$start.ExitCode;startOutput=$start.Output;startError=$start.ErrorOutput}|ConvertTo-Json
