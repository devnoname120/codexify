# Dot-sourced by the coordinated updater. Never terminate a live Chrome profile.
function Test-CleanBrowserStopExitRace([string]$BrowserRoot,[string]$DescriptorPath,[int]$OriginalLauncherPid,[string]$StopErrorPath) {
  # Native Stop can receive a health reply just before the gracefully closing
  # launcher exits, then fail its PID/path lookup. Accept only that exact error
  # plus independently verified absence of EVERY managed browser process.
  if(-not(Test-Path -LiteralPath $StopErrorPath)){return $false}
  if(([string](Get-Content -LiteralPath $StopErrorPath -Raw)).Trim() -ne 'Error: Could not verify the browser companion executable'){return $false}
  if(Get-CimInstance Win32_Process -Filter ("ProcessId="+$OriginalLauncherPid)){return $false}
  $runtime=[IO.Path]::GetFullPath((Join-Path $BrowserRoot 'runtime')).TrimEnd('\')+'\'
  $remaining=@(Get-CimInstance Win32_Process | Where-Object {$_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($runtime,[StringComparison]::OrdinalIgnoreCase)})
  if($remaining.Count){return $false}
  if(Test-Path -LiteralPath $DescriptorPath){
    try {$current=Get-Content -LiteralPath $DescriptorPath -Raw|ConvertFrom-Json;if([int]$current.pid -ne $OriginalLauncherPid){return $false}}catch{return $false}
  }
  return $true
}
function Stop-ManagedBrowserGracefully([string]$BrowserRoot,[string]$NativeExe,[string]$BackupDirectory,[string]$ExpectedAccountFingerprint) {
  $ErrorActionPreference='Stop'
  $descriptorPath=Join-Path $BrowserRoot 'descriptor.json'
  $descriptor=Get-Content -LiteralPath $descriptorPath -Raw | ConvertFrom-Json
  $runtime=[IO.Path]::GetFullPath((Join-Path $BrowserRoot 'runtime')).TrimEnd('\')+'\'
  $profile=[IO.Path]::GetFullPath((Join-Path $BrowserRoot 'chrome-profile'))
  $endpoint=[uri]$descriptor.endpoint
  if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1'){throw 'Unexpected browser endpoint'}
  $headers=@{Authorization=('Bearer '+$descriptor.token)}
  $until=[DateTime]::UtcNow.AddSeconds(30)
  do {
    $health=Invoke-RestMethod ($descriptor.endpoint.TrimEnd('/')+'/v1/health') -Headers $headers -TimeoutSec 15
    if($health.authenticated -and $health.status -eq 'idle' -and -not $health.activeActionId){break}
    Start-Sleep -Milliseconds 250
  }while([DateTime]::UtcNow -lt $until)
  if(-not $health.authenticated -or $health.status -ne 'idle' -or $health.activeActionId){throw 'Browser must be authenticated and idle before clean shutdown'}
  if($ExpectedAccountFingerprint -and $health.accountFingerprint -ne $ExpectedAccountFingerprint){throw 'Browser account changed before the update'}
  $account=$health.accountFingerprint
  $chrome=@(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" | Where-Object {
    $_.ParentProcessId -eq [int]$descriptor.pid -and $_.ExecutablePath -and
    [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($runtime,[StringComparison]::OrdinalIgnoreCase) -and
    $_.CommandLine.Contains($profile) -and $_.CommandLine -notlike '*--type=*'
  })
  if($chrome.Count -ne 1){throw 'Could not verify exactly one managed Chrome profile owner'}
  $chromeId=[int]$chrome[0].ProcessId
  $owner=Get-Process -Id $chromeId
  if(-not $owner.MainWindowHandle){throw 'Managed Chrome has no window for a clean close; live browser preserved'}
  $lifecycle=Join-Path $BrowserRoot 'lifecycle.json'
  $previous=$true
  if(Test-Path -LiteralPath $lifecycle){$previous=(Get-Content -LiteralPath $lifecycle -Raw|ConvertFrom-Json).automaticStart}
  $lock=$null;$locked=$false;$stop=$null;$success=$false
  try {
    $lock=[IO.File]::Open((Join-Path $BrowserRoot 'lifecycle.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    do {try{$lock.Lock(0,1);$locked=$true}catch{Start-Sleep -Milliseconds 100}}while(-not $locked -and [DateTime]::UtcNow -lt $deadline)
    if(-not $locked){throw 'Another browser lifecycle operation owns the profile'}
    # The supported Stop command records automaticStart=false before waiting
    # for this lock. It cannot force-stop Chrome while we close it normally.
    $stop=Start-Process -FilePath $NativeExe -ArgumentList @('browser','stop') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $BackupDirectory 'graceful-stop.stdout') -RedirectStandardError (Join-Path $BackupDirectory 'graceful-stop.stderr')
    # Windows PowerShell 5.1 reports a null ExitCode unless the handle is opened early.
    $null=$stop.Handle
    $deadline=[DateTime]::UtcNow.AddSeconds(10);$disabled=$false
    do {
      if(Test-Path -LiteralPath $lifecycle){$disabled=-not (Get-Content -LiteralPath $lifecycle -Raw|ConvertFrom-Json).automaticStart}
      if(-not $disabled){Start-Sleep -Milliseconds 100}
    }while(-not $disabled -and [DateTime]::UtcNow -lt $deadline)
    if(-not $disabled){throw 'Native browser stop did not settle its lifecycle intent'}
    $until=[DateTime]::UtcNow.AddSeconds(15)
    do {
      $fresh=Invoke-RestMethod ($descriptor.endpoint.TrimEnd('/')+'/v1/health') -Headers $headers -TimeoutSec 15
      if($fresh.authenticated -and $fresh.accountFingerprint -eq $account -and $fresh.status -eq 'idle' -and -not $fresh.activeActionId){break}
      Start-Sleep -Milliseconds 250
    }while([DateTime]::UtcNow -lt $until)
    if(-not $fresh.authenticated -or $fresh.accountFingerprint -ne $account -or $fresh.status -ne 'idle' -or $fresh.activeActionId){throw 'Browser activity or account changed before clean shutdown; browser preserved'}
    $current=Get-CimInstance Win32_Process -Filter "ProcessId=$chromeId"
    if($current.ExecutablePath -ine $chrome[0].ExecutablePath -or $current.ParentProcessId -ne $descriptor.pid){throw 'Managed Chrome identity changed'}
    $owner.Refresh()
    if(-not $owner.CloseMainWindow()){throw 'Chrome declined clean shutdown; no forced termination attempted'}
    if(-not $owner.WaitForExit(45000)){
      # Closing the last window can leave Chrome's background process alive.
      # Accept that only when no window or renderer remains, then allow a
      # further 30 seconds for profile and cookie writes before native stop.
      $owner.Refresh()
      $renderers=@(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" | Where-Object {$_.ParentProcessId -eq $chromeId -and $_.CommandLine -like '*--type=renderer*'})
      if($owner.MainWindowHandle -ne [IntPtr]::Zero -or $renderers.Count){throw 'Chrome did not close cleanly; no forced termination attempted'}
      if(-not $owner.WaitForExit(30000)){
        @{windowClosed=$true;backgroundProcessRetained=$chromeId;atUtc=[DateTime]::UtcNow.ToString('o')}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $BackupDirectory 'chrome-background-after-close.json')
      }
    }
    $launcher=Get-Process -Id ([int]$descriptor.pid) -ErrorAction SilentlyContinue
    # Chrome has saved and closed its profile. The native stop command owns
    # launcher shutdown and must acquire this lock before it can finish.
    # Waiting for the launcher while holding the lock deadlocks that command.
    $lock.Unlock(0,1);$locked=$false
    if(-not $stop.WaitForExit(15000)){throw 'Native stop receipt did not finish after clean shutdown'}
    $stop.Refresh()
    if($stop.ExitCode -ne 0){
      $settle=[DateTime]::UtcNow.AddSeconds(30);$gone=$false
      do {
        $gone=Test-CleanBrowserStopExitRace -BrowserRoot $BrowserRoot -DescriptorPath $descriptorPath -OriginalLauncherPid ([int]$descriptor.pid) -StopErrorPath (Join-Path $BackupDirectory 'graceful-stop.stderr')
        if(-not $gone){Start-Sleep -Milliseconds 250}
      }while(-not $gone -and [DateTime]::UtcNow -lt $settle)
      if(-not $gone){throw 'Native stop rejected the clean-shutdown receipt'}
      @{verifiedGone=$true;originalLauncherPid=$descriptor.pid;atUtc=[DateTime]::UtcNow.ToString('o')}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $BackupDirectory 'native-stop-clean-exit-race.json')
    }
    if($launcher -and -not $launcher.WaitForExit(15000)){throw 'Browser launcher did not finish after the native stop receipt'}
    $target=Join-Path $BackupDirectory 'browser-profile'
    if(Test-Path -LiteralPath $target){throw 'Browser profile backup slot already exists'}
    Copy-Item -LiteralPath $profile -Destination $target -Recurse
    foreach($relative in @('Local State','Default/Preferences','Default/Network/Cookies')){
      $source=Join-Path $profile $relative;$copy=Join-Path $target $relative
      if((Test-Path -LiteralPath $source) -and (Get-FileHash -LiteralPath $source).Hash -ne (Get-FileHash -LiteralPath $copy).Hash){throw 'Closed browser profile backup failed verification'}
    }
    $success=$true
    return @{graceful=$true;profileBackupVerified=$true;chromePid=$chromeId}
  } finally {
    if(-not $success -and $stop -and -not $stop.HasExited){
      $client=Get-CimInstance Win32_Process -Filter ("ProcessId="+$stop.Id)
      if($client.ExecutablePath -ieq $NativeExe){Stop-Process -Id $stop.Id}
    }
    if($locked){$lock.Unlock(0,1)}
    if($lock){$lock.Dispose()}
    if(-not $success -and $previous){
      # Start is idempotent for a healthy existing browser; restore the prior
      # supported lifecycle choice after an aborted maintenance attempt.
      $restore=Start-Process -FilePath $NativeExe -ArgumentList @('browser','start') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $BackupDirectory 'restore-browser.stdout') -RedirectStandardError (Join-Path $BackupDirectory 'restore-browser.stderr')
      $null=$restore.WaitForExit(30000)
    }
  }
}
