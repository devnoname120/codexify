param(
  [Parameter(Mandatory=$true)][string]$Archive,
  [Parameter(Mandatory=$true)][string]$RollbackArchive,
  [Parameter(Mandatory=$true)][string]$Backup,
  [switch]$Deploy
)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-agent-update-guards.ps1')
$ProgressPreference='SilentlyContinue'
$root=Join-Path $env:USERPROFILE '.codexify'
$exe=Join-Path $root 'bin\codexify.exe'
$counter=0
$servers=@()
$stopped=$false
$installed=$false
$previousReleaseRoot=$env:CODEXIFY_BROWSER_RELEASE_ROOT
New-Item -ItemType Directory -Path $Backup -Force | Out-Null
function Record([string]$Stage,[string]$Detail) {
  @{stage=$Stage;detail=$Detail;atUtc=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json -Compress |
    Set-Content -LiteralPath (Join-Path $Backup 'browser-patch-status.json')
  Write-Output "$Stage`: $Detail"
}
function Read-SharedJson([string]$Path) {
  $stream=[IO.File]::Open($Path,[IO.FileMode]::Open,[IO.FileAccess]::Read,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
  $reader=[IO.StreamReader]::new($stream)
  try {return $reader.ReadToEnd() | ConvertFrom-Json} finally {$reader.Dispose()}
}
function Invoke-Cli([string[]]$Arguments) {
  $script:counter++
  $out=Join-Path $Backup "cli-$counter.stdout"
  $err=Join-Path $Backup "cli-$counter.stderr"
  $process=Start-Process -FilePath $exe -ArgumentList $Arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
  # Windows PowerShell 5.1 reports a null ExitCode unless the handle is opened early.
  $null=$process.Handle
  if(-not $process.WaitForExit(180000)){throw "CLI timed out: $($Arguments -join ' ')"}
  $process.Refresh()
  if($process.ExitCode -ne 0){throw "CLI failed: $($Arguments -join ' '); see $err"}
  $stream=[IO.File]::Open($out,[IO.FileMode]::Open,[IO.FileAccess]::Read,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
  $reader=[IO.StreamReader]::new($stream)
  try {return $reader.ReadToEnd()} finally {$reader.Dispose()}
}
function Start-ReleaseServer([string]$Path,[string]$Label) {
  $ready=Join-Path $Backup ($Label+'-release.json')
  if(Test-Path -LiteralPath $ready){throw 'Release server ready slot already used'}
  $serverScript=Join-Path $PSScriptRoot 'windows-update-file-server.cjs'
  $process=Start-Process -FilePath (Get-Command node.exe).Source -ArgumentList @(('"'+$serverScript+'"'),('"'+$Path+'"'),('"'+$ready+'"')) -WindowStyle Hidden -PassThru
  $script:servers+=@($process)
  for($i=0;$i -lt 80 -and -not(Test-Path -LiteralPath $ready);$i++){Start-Sleep -Milliseconds 250}
  if(-not(Test-Path -LiteralPath $ready)){throw 'Local release server did not start'}
  return Read-SharedJson $ready
}
function Browser-Idle {
  try {
    $path=Join-Path $root 'browser\descriptor.json'
    if(-not(Test-Path -LiteralPath $path)){return $true}
    $descriptor=Read-SharedJson $path
    if(-not(Get-CimInstance Win32_Process -Filter ("ProcessId="+[int]$descriptor.pid))){return $true}
    $endpoint=[uri]$descriptor.endpoint
    if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1'){return $false}
    $health=Invoke-RestMethod ($descriptor.endpoint.TrimEnd('/')+'/v1/health') -TimeoutSec 10 -Headers @{Authorization=('Bearer '+$descriptor.token)}
    if(-not (Test-BrowserPoolUnowned $health)){return $false}
    if($health.authenticated -and $health.status -eq 'unavailable'){
      # An idle signed-in page can be unusable during hydration/rate limits.
      # Verify the control surface is live before replacing that broken runtime.
      $liveness=Invoke-RestMethod ($descriptor.endpoint.TrimEnd('/')+'/v1/liveness') -TimeoutSec 5 -Headers @{Authorization=('Bearer '+$descriptor.token)}
      return [bool]$liveness.available
    }
    return $health.status -in @('idle','authentication_required')
  } catch {return $false}
}
function Wait-BrowserReady([string]$Version) {
  $deadline=[DateTime]::UtcNow.AddSeconds(60)
  do {
    try {
      $descriptor=Read-SharedJson (Join-Path $root 'browser\descriptor.json')
      $endpoint=[uri]$descriptor.endpoint
      if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1'){throw 'Unexpected browser endpoint'}
      $health=Invoke-RestMethod ($descriptor.endpoint.TrimEnd('/')+'/v1/health') -TimeoutSec 5 -Headers @{Authorization=('Bearer '+$descriptor.token)}
      if($health.authenticated -and $health.status -in @('idle','busy') -and $health.build.StartsWith($Version+'+protocol.')){return}
    } catch {}
    Start-Sleep -Seconds 1
  } while([DateTime]::UtcNow -lt $deadline)
  throw 'Browser did not finish startup and account verification before the readiness deadline'
}
function Wait-BrowserStopped {
  $runtime=[IO.Path]::GetFullPath((Join-Path $root 'browser\runtime')).TrimEnd('\')+'\'
  $deadline=[DateTime]::UtcNow.AddSeconds(30)
  do {
    $remaining=@(Get-CimInstance Win32_Process | Where-Object {$_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($runtime,[StringComparison]::OrdinalIgnoreCase)})
    if(-not $remaining.Count){Start-Sleep -Milliseconds 750;return}
    Start-Sleep -Seconds 1
  }while([DateTime]::UtcNow -lt $deadline)
  throw 'Private browser processes are still exiting; runtime files were preserved'
}
function Install-WithRetry {
  for($attempt=0;$attempt -lt 3;$attempt++){
    try {$null=Invoke-Cli @('browser','install');return}
    catch {if($attempt -eq 2){throw};Start-Sleep -Seconds 3}
  }
}
try {
  foreach($path in @($Archive,$RollbackArchive,$exe)){if(-not(Test-Path -LiteralPath $path -PathType Leaf)){throw "Missing artifact: $path"}}
  if((Get-FileHash -LiteralPath $Archive).Hash -eq (Get-FileHash -LiteralPath $RollbackArchive).Hash){throw 'Patch and rollback packages must differ'}
  $listener=Get-NetTCPConnection -State Listen -LocalPort 3000 | Select-Object -First 1
  $live=Get-CimInstance Win32_Process -Filter "ProcessId=$($listener.OwningProcess)"
  if($live.ExecutablePath -ine $exe){throw 'Unexpected live server executable'}
  Record 'preflight_verified' 'Browser packages and live server verified'
  if(-not $Deploy){return}
  $release=Start-ReleaseServer $Archive 'patch'
  $env:CODEXIFY_BROWSER_RELEASE_ROOT=$release.root
  Record 'waiting_for_idle' 'Waiting for the shared browser to release action ownership'
  $ready=$false
  for($i=0;$i -lt 150;$i++){
    if(Browser-Idle){$ready=$true;break}
    Start-Sleep -Seconds 2
  }
  if(-not $ready){throw 'Browser did not become idle; live work was not interrupted'}
  $stopped=$true
  try {$null=Invoke-Cli @('browser','stop')}
  catch {
    # Windows may report the shutdown deadline just before Electron exits.
    # Verify every private runtime process actually left before proceeding.
    Wait-BrowserStopped
  }
  Wait-BrowserStopped
  Record 'installing_browser' 'Installing checksum-verified browser patch; native server stays running'
  Install-WithRetry
  $installed=$true
  $null=Invoke-Cli @('browser','start')
  Wait-BrowserReady $release.version
  if(-not(Get-CimInstance Win32_Process -Filter "ProcessId=$($live.ProcessId)")){throw 'Native server changed during browser patch'}
  Record 'completed' ('Browser '+$release.version+' patch installed; native server and agent commands preserved')
} catch {
  $failure=$_.Exception.Message
  Record 'failed' $failure
  if($stopped){
    try {
      if($installed){
        $rollback=Start-ReleaseServer $RollbackArchive 'rollback'
        $env:CODEXIFY_BROWSER_RELEASE_ROOT=$rollback.root
        $null=Invoke-Cli @('browser','stop')
        Wait-BrowserStopped
        Install-WithRetry
      }
      $null=Invoke-Cli @('browser','start')
      Record 'rolled_back' 'Previous browser package restored; native server and chat state retained'
    } catch {Record 'rollback_failed' $_.Exception.Message}
  }
  throw $failure
} finally {
  $env:CODEXIFY_BROWSER_RELEASE_ROOT=$previousReleaseRoot
  foreach($process in $servers){if(-not $process.HasExited){Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue}}
}
