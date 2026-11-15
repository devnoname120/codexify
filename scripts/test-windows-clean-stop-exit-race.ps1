$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-browser-graceful-update.ps1')
$fixture=Join-Path ([IO.Path]::GetTempPath()) ('clean-stop-race-test-'+[Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$descriptor=Join-Path $fixture 'descriptor.json';$errorFile=Join-Path $fixture 'stderr.txt'
@{pid=42000}|ConvertTo-Json|Set-Content -LiteralPath $descriptor
Set-Content -LiteralPath $errorFile -Value 'Error: Could not verify the browser companion executable'
$script:livePid=$false;$script:otherProcess=$false
function Get-CimInstance {
 param($ClassName,$Filter)
 if($Filter -eq 'ProcessId=42000' -and $script:livePid){return [pscustomobject]@{ProcessId=42000;ExecutablePath=(Join-Path $fixture 'runtime/browser.exe')}}
 if(-not $Filter -and $script:otherProcess){return [pscustomobject]@{ProcessId=42001;ExecutablePath=(Join-Path $fixture 'runtime/browser.exe')}}
}
$check={Test-CleanBrowserStopExitRace $fixture $descriptor 42000 $errorFile}
if(-not(& $check)){throw 'Exact clean-exit race was rejected'}
$script:livePid=$true;if(& $check){throw 'A live original PID was accepted'};$script:livePid=$false
$script:otherProcess=$true;if(& $check){throw 'A remaining managed process was accepted'};$script:otherProcess=$false
@{pid=42001}|ConvertTo-Json|Set-Content -LiteralPath $descriptor
if(& $check){throw 'Replacement descriptor was accepted'}
@{pid=42000}|ConvertTo-Json|Set-Content -LiteralPath $descriptor
Set-Content -LiteralPath $errorFile -Value 'Error: Another genuine failure'
if(& $check){throw 'Unrelated failure was accepted'}
[ordered]@{ok=$true;exactErrorAndGoneOnly=$true;livePidRejected=$true;remainingManagedProcessRejected=$true;replacementDescriptorRejected=$true;otherFailureRejected=$true;fixture=$fixture}|ConvertTo-Json -Compress
