#Requires -Version 7.0
$ErrorActionPreference='Stop'
$base=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')+'\'
$fixture=Join-Path $base ('codexify-stage-on-exit-'+[Guid]::NewGuid().ToString('N'))
$root=Join-Path $fixture 'app'
$browser=Join-Path $root 'browser'
$archive=Join-Path $browser 'runtime/fixture/app/resources/app.asar'
$candidate=Join-Path $fixture 'candidate.asar'
$receiptPath=Join-Path $fixture 'receipt.json'
$activationPath=Join-Path $fixture 'activation.json'
$scriptPath=Join-Path $PSScriptRoot 'windows-stage-browser-patch-on-exit.ps1'
$global:StageFixtureProcessCalls=0
$global:StageFixtureMode='idle'
$global:StageFixtureRoot=$browser
function Get-CimInstance {
  param($ClassName)
  $global:StageFixtureProcessCalls++
  if($global:StageFixtureMode -eq 'exiting'){
    [pscustomobject]@{ProcessId=$global:StageFixtureChildId;Name='codexify-browser-companion.exe';ExecutablePath=(Join-Path $global:StageFixtureRoot 'runtime/fixture/app/codexify-browser-companion.exe')}
    return
  }
  if($global:StageFixtureMode -eq 'busy' -or
      ($global:StageFixtureMode -eq 'started_during_lock' -and $global:StageFixtureProcessCalls -gt 1)){
    [pscustomobject]@{ProcessId=$PID;Name='codexify-browser-companion.exe';ExecutablePath=(Join-Path $global:StageFixtureRoot 'runtime/fixture/app/codexify-browser-companion.exe')}
  }
}
function Invoke-Fixture {
  $global:StageFixtureProcessCalls=0
  & $scriptPath -Root $root -ReceiptPath $receiptPath -ActivationPath $activationPath | ConvertFrom-Json
}
function Reset-Fixture {
  [IO.File]::WriteAllText($archive,'old archive')
  [IO.File]::WriteAllText($candidate,'new archive')
  [IO.File]::WriteAllText((Join-Path $browser 'lifecycle.json'),'{"automaticStart":true}')
  @{version='fixture';live=$archive;candidate=$candidate;previousSha256=(Get-FileHash -LiteralPath $archive).Hash;sha256=(Get-FileHash -LiteralPath $candidate).Hash} |
    ConvertTo-Json | Set-Content -LiteralPath $receiptPath
  $global:StageFixtureMode='idle'
}
try {
  $null=New-Item -ItemType Directory -Path (Split-Path -Parent $archive)
  [IO.File]::WriteAllText((Join-Path $browser 'lifecycle.lock'),'')
  Reset-Fixture
  $global:StageFixtureMode='busy'
  if((Invoke-Fixture).state -ne 'waiting_for_browser_exit'){throw 'A running browser must defer staging'}
  if((Get-Content -LiteralPath $archive -Raw) -ne 'old archive'){throw 'Busy staging changed the archive'}
  $global:StageFixtureMode='started_during_lock'
  if((Invoke-Fixture).state -ne 'waiting_for_browser_exit'){throw 'A replacement starting before the lock was ignored'}
  Reset-Fixture
  [IO.File]::WriteAllText((Join-Path $browser 'lifecycle.json'),'{"automaticStart":false}')
  if((Invoke-Fixture).state -ne 'browser_explicitly_stopped'){throw 'An explicit stop must retain its state'}
  Reset-Fixture
  $watchLock=[IO.File]::Open($activationPath+'.watch.lock',[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
  try {if((Invoke-Fixture).state -ne 'watcher_already_running'){throw 'A second watcher acquired the same receipt'}}
  finally {$watchLock.Dispose()}
  Reset-Fixture
  $lifecycleLock=[IO.File]::Open((Join-Path $browser 'lifecycle.lock'),[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite,[IO.FileShare]::ReadWrite)
  $lifecycleLock.Lock(0,1)
  try {if((Invoke-Fixture).state -ne 'waiting_for_lifecycle_lock'){throw 'An owned native lifecycle lock must defer staging'}}
  finally {$lifecycleLock.Unlock(0,1);$lifecycleLock.Dispose()}
  Reset-Fixture
  [IO.File]::WriteAllText($candidate,'changed candidate')
  $rejected=$false
  try {$null=Invoke-Fixture} catch {$rejected=$true}
  if(-not $rejected -or (Get-Content -LiteralPath $archive -Raw) -ne 'old archive'){throw 'A changed candidate was not rejected'}
  Reset-Fixture
  $result=Invoke-Fixture
  if($result.state -ne 'staged_for_next_launch'){throw 'An idle stopped runtime was not staged'}
  if((Get-Content -LiteralPath $archive -Raw) -ne 'new archive' -or (Get-Content -LiteralPath $result.backup -Raw) -ne 'old archive'){throw 'Staging or rollback archive verification failed'}
  if((Get-Content -LiteralPath (Join-Path $browser 'lifecycle.json') -Raw|ConvertFrom-Json).automaticStart -ne $true){throw 'Staging changed the lifecycle preference'}
  $again=Invoke-Fixture
  if($again.state -ne 'already_staged'){throw 'Identical staging was not idempotent'}
  if($again.backup -ne $result.backup){throw 'Idempotent verification discarded the rollback receipt'}
  Reset-Fixture
  $child=Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList @('-NoProfile','-Command','Start-Sleep -Milliseconds 1500') -WindowStyle Hidden -PassThru
  try {
    $global:StageFixtureChildId=$child.Id
    $global:StageFixtureMode='exiting'
    & $scriptPath -Root $root -ReceiptPath $receiptPath -ActivationPath $activationPath -Watch -MaxMinutes 1 | Out-Null
    $finished=Get-Content -LiteralPath $activationPath -Raw|ConvertFrom-Json
    if($finished.state -ne 'staged_for_next_launch' -or (Get-Content -LiteralPath $archive -Raw) -ne 'new archive'){
      throw 'The watcher did not stage after its observed process exited'
    }
  } finally {$null=$child.WaitForExit(4000);$child.Dispose()}
  'PASS: 9 staging cases covering active processes, launch races, lifecycle ownership, explicit stop, duplicate watcher, changed candidate, backup, idempotency and natural exit'
} finally {
  Remove-Item -LiteralPath function:Get-CimInstance
  Remove-Variable -Name StageFixtureProcessCalls,StageFixtureMode,StageFixtureRoot,StageFixtureChildId -Scope Global -ErrorAction SilentlyContinue
  if(Test-Path -LiteralPath $fixture){
    $resolved=(Resolve-Path -LiteralPath $fixture).Path
    if(-not $resolved.StartsWith($base,[StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolved) -notlike 'codexify-stage-on-exit-*'){throw 'Unsafe fixture cleanup path'}
    Remove-Item -LiteralPath $resolved -Recurse -Force
  }
}
