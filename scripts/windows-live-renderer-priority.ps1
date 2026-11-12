#Requires -Version 7.0
param([ValidateRange(60,14400)][int]$DurationSeconds=3600)
$ErrorActionPreference='Stop'
$browserRoot=Join-Path $env:USERPROFILE '.codexify/browser'
$descriptorPath=Join-Path $browserRoot 'descriptor.json'
$runtimePrefix=[IO.Path]::GetFullPath((Join-Path $browserRoot 'runtime')).TrimEnd('\')+'\'
$descriptor=Get-Content -LiteralPath $descriptorPath -Raw|ConvertFrom-Json
$browserId=[uint32]$descriptor.pid
$owner=Get-CimInstance Win32_Process -Filter ('ProcessId='+$browserId)
if(-not $owner.ExecutablePath -or -not $owner.ExecutablePath.StartsWith($runtimePrefix,[StringComparison]::OrdinalIgnoreCase) `
  -or $owner.Name -ne 'codexify-browser-companion.exe'){throw 'Private companion identity could not be verified'}
$started=([DateTimeOffset]$owner.CreationDate.ToUniversalTime()).ToUnixTimeMilliseconds()
$mutex=[Threading.Mutex]::new($false,('Local\CodexifyRendererPriority-'+$browserId+'-'+$started))
if(-not $mutex.WaitOne(0)){@{state='already_running';browserPid=$browserId}|ConvertTo-Json;exit 0}
$statusPath=Join-Path (Split-Path -Parent $PSScriptRoot) 'browser-companion/artifacts/ready-20261030/renderer-priority-maintenance.json'
$deadline=[DateTime]::UtcNow.AddSeconds($DurationSeconds)
$changed=0;$lastChanges=@();$lastSave=[datetime]::MinValue;$reason='deadline'
try {
  while([datetime]::UtcNow -lt $deadline){
    $current=Get-Content -LiteralPath $descriptorPath -Raw|ConvertFrom-Json
    $lifecycle=Get-Content -LiteralPath (Join-Path $browserRoot 'lifecycle.json') -Raw|ConvertFrom-Json
    if($current.pid -ne $browserId -or -not $lifecycle.automaticStart){$reason='browser_changed_or_stopped';break}
    $running=Get-Process -Id $browserId -ErrorAction SilentlyContinue
    if(-not $running -or ([DateTimeOffset]$running.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds() -ne $started){$reason='browser_exited';break}
    $chrome=@(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'")
    $main=@($chrome|Where-Object {$_.ParentProcessId -eq $browserId -and $_.ExecutablePath `
      -and $_.ExecutablePath.StartsWith($runtimePrefix,[StringComparison]::OrdinalIgnoreCase) `
      -and $_.CommandLine.Contains((Join-Path $browserRoot 'chrome-profile'))})
    foreach($hostProcess in $main){
      foreach($renderer in @($chrome|Where-Object {$_.ParentProcessId -eq $hostProcess.ProcessId `
        -and $_.ExecutablePath -ieq $hostProcess.ExecutablePath -and $_.CommandLine -match '--type=renderer(?:\s|$)'})){
        try {
          $process=Get-Process -Id $renderer.ProcessId -ErrorAction Stop
          if(([DateTimeOffset]$process.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds() -ne ([DateTimeOffset]$renderer.CreationDate.ToUniversalTime()).ToUnixTimeMilliseconds()){continue}
          if($process.PriorityClass -in @('Idle','BelowNormal')){
            $before=[string]$process.PriorityClass;$process.PriorityClass='Normal';$changed++
            $lastChanges=@($lastChanges|Select-Object -Last 7)+@(@{pid=$process.Id;before=$before;after='Normal';atUtc=[datetime]::UtcNow.ToString('o')})
          }
        }catch{}
      }
    }
    if(([datetime]::UtcNow-$lastSave).TotalSeconds -ge 10){
      $lastSave=[datetime]::UtcNow
      $status=@{state='running';pid=$PID;browserPid=$browserId;browserStartedAtMs=$started;changedCount=$changed;recentChanges=$lastChanges;checkedAtUtc=$lastSave.ToString('o');deadlineUtc=$deadline.ToString('o')}
      [IO.File]::WriteAllText($statusPath,($status|ConvertTo-Json -Depth 4))
    }
    Start-Sleep -Seconds 2
  }
}finally{
  [IO.File]::WriteAllText($statusPath,(@{state='stopped';pid=$PID;browserPid=$browserId;reason=$reason;changedCount=$changed;checkedAtUtc=[datetime]::UtcNow.ToString('o')}|ConvertTo-Json))
  $mutex.ReleaseMutex();$mutex.Dispose()
}
