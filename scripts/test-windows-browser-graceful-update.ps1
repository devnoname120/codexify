param([string]$HelperPath)
$ErrorActionPreference='Stop'
if(-not $HelperPath){$HelperPath=Join-Path $PSScriptRoot 'windows-browser-graceful-update.ps1'}
. $HelperPath
$script:fixtureRoot=Join-Path 'D:/CodexifyBuild' ('graceful-lock-test-'+[guid]::NewGuid().ToString('N'))
$script:fixtureBackup=Join-Path $script:fixtureRoot 'backup'
$script:fixtureNative=Join-Path $script:fixtureRoot 'native.exe'
$script:fixtureChrome=Join-Path $script:fixtureRoot 'runtime/chrome.exe'
$script:fixtureStop=$null
New-Item -ItemType Directory -Path $script:fixtureBackup,(Join-Path $script:fixtureRoot 'runtime'),(Join-Path $script:fixtureRoot 'chrome-profile/Default/Network') -Force|Out-Null
@{endpoint='http://127.0.0.1:1';token='test-only';pid=42000}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $script:fixtureRoot 'descriptor.json')
@{automaticStart=$true}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $script:fixtureRoot 'lifecycle.json')
foreach($relative in @('Local State','Default/Preferences','Default/Network/Cookies')){
  Set-Content -LiteralPath (Join-Path $script:fixtureRoot ('chrome-profile/'+$relative)) -Value 'disposable profile fixture'
}
$stopScript=Join-Path $script:fixtureRoot 'native-stop.ps1'
@'
param([string]$Root)
$ErrorActionPreference='Stop'
@{automaticStart=$false}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $Root 'lifecycle.json')
$file=[IO.File]::Open((Join-Path $Root 'lifecycle.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
$locked=$false
try {
  $deadline=[DateTime]::UtcNow.AddSeconds(20)
  do {try{$file.Lock(0,1);$locked=$true}catch{Start-Sleep -Milliseconds 25}}while(-not $locked -and [DateTime]::UtcNow -lt $deadline)
  if(-not $locked){throw 'Stop could not acquire lifecycle lock'}
  if(-not(Test-Path -LiteralPath (Join-Path $Root 'chrome-closed'))){throw 'Stop ran before Chrome closed gracefully'}
  Set-Content -LiteralPath (Join-Path $Root 'stop-complete') -Value 'native stop owns launcher shutdown'
} finally {if($locked){$file.Unlock(0,1)};$file.Dispose()}
'@|Set-Content -LiteralPath $stopScript
function Invoke-RestMethod {return @{authenticated=$true;status='idle';activeActionId=$null;accountFingerprint='fixture-account'}}
function Get-CimInstance {
  param($ClassName,$Filter)
  if($Filter -eq "Name='chrome.exe'" -or $Filter -eq 'ProcessId=42001'){
    return [pscustomobject]@{ProcessId=42001;ParentProcessId=42000;ExecutablePath=$script:fixtureChrome;CommandLine=($script:fixtureChrome+' '+(Join-Path $script:fixtureRoot 'chrome-profile'))}
  }
  if($script:fixtureStop -and $Filter -eq ('ProcessId='+$script:fixtureStop.Id)){return [pscustomobject]@{ExecutablePath=$script:fixtureNative}}
}
function Get-Process {
  param($Id,$ErrorAction)
  if($Id -eq 42001){
    $p=[pscustomobject]@{MainWindowHandle=1}
    $p|Add-Member ScriptMethod Refresh {}
    $p|Add-Member ScriptMethod CloseMainWindow {Set-Content -LiteralPath (Join-Path $script:fixtureRoot 'chrome-closed') -Value 'clean close';return $true}
    $p|Add-Member ScriptMethod WaitForExit {param($timeout);return $true}
    return $p
  }
  if($Id -eq 42000){
    $p=[pscustomobject]@{}
    $p|Add-Member ScriptMethod WaitForExit {param($timeout);return (Test-Path -LiteralPath (Join-Path $script:fixtureRoot 'stop-complete'))}
    return $p
  }
}
function Start-Process {
  param($FilePath,$ArgumentList,$WindowStyle,[switch]$PassThru,$RedirectStandardOutput,$RedirectStandardError)
  if($ArgumentList[1] -eq 'start'){
    $p=[pscustomobject]@{}
    $p|Add-Member ScriptMethod WaitForExit {param($timeout);return $true}
    return $p
  }
  if($ArgumentList[1] -ne 'stop'){throw 'Unexpected lifecycle command'}
  $script:fixtureStop=Microsoft.PowerShell.Management\Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') -ArgumentList @('-NoProfile','-File',('"'+$stopScript+'"'),('-Root "'+$script:fixtureRoot+'"')) -WindowStyle Hidden -PassThru -RedirectStandardOutput $RedirectStandardOutput -RedirectStandardError $RedirectStandardError
  return $script:fixtureStop
}
try {
  $result=Stop-ManagedBrowserGracefully -BrowserRoot $script:fixtureRoot -NativeExe $script:fixtureNative -BackupDirectory $script:fixtureBackup -ExpectedAccountFingerprint 'fixture-account'
  if(-not $result.graceful -or -not $result.profileBackupVerified){throw 'Missing verified shutdown receipt'}
  if(-not(Test-Path -LiteralPath (Join-Path $script:fixtureRoot 'stop-complete'))){throw 'Native stop did not finish'}
  if(-not(Test-Path -LiteralPath (Join-Path $script:fixtureBackup 'browser-profile/Default/Network/Cookies'))){throw 'Closed profile was not retained'}
  [pscustomobject]@{passed=$true;realCrossProcessLifecycleLock=$true;chromeClosedBeforeNativeStop=$true;launcherStoppedAfterNativeReceipt=$true;profileBackupVerified=$true;fixture=$script:fixtureRoot}|ConvertTo-Json
} finally {
  if($script:fixtureStop -and -not $script:fixtureStop.HasExited){Stop-Process -InputObject $script:fixtureStop -Force}
}
