$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-browser-graceful-update.ps1')
$fixture=Join-Path ([IO.Path]::GetTempPath()) ('chrome-profile-exit-'+[Guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $fixture
$script:renderers=@()
function Get-CimInstance {param($ClassName,$Filter) $script:renderers}
function New-Owner([bool[]]$Exits,[int]$Window=0) {
  $owner=[pscustomobject]@{MainWindowHandle=[IntPtr]$Window;Exits=$Exits;Waits=[Collections.Generic.List[int]]::new()}
  $owner|Add-Member ScriptMethod Refresh {}
  $owner|Add-Member ScriptMethod WaitForExit {
    param($Timeout)
    $index=$this.Waits.Count;$this.Waits.Add($Timeout)
    return $this.Exits[$index]
  }
  return $owner
}
try {
  $owner=New-Owner @($true)
  Wait-ManagedChromeProfileExit $owner 42001 $fixture
  if($owner.Waits.Count -ne 1){throw 'A closed owner must not receive another wait'}
  $owner=New-Owner @($false,$true)
  Wait-ManagedChromeProfileExit $owner 42001 $fixture
  if(($owner.Waits -join ',') -ne '45000,30000'){throw 'Background exit must have a bounded grace period'}
  foreach($visible in @($false,$true)){
    $owner=New-Owner @($false,$false) $(if($visible){1}else{0})
    $rejected=$false
    try {Wait-ManagedChromeProfileExit $owner 42001 $fixture}catch{$rejected=$true}
    if(-not $rejected){throw 'A live profile owner was accepted for native teardown'}
  }
  if(-not(Test-Path -LiteralPath (Join-Path $fixture 'chrome-background-after-close.json'))){throw 'Missing background-owner deferral receipt'}
  $script:renderers=@([pscustomobject]@{ParentProcessId=42001;CommandLine='chrome.exe --type=renderer'})
  $owner=New-Owner @($false,$false)
  $rejected=$false
  try {Wait-ManagedChromeProfileExit $owner 42001 $fixture}catch{$rejected=$true}
  if(-not $rejected -or $owner.Waits.Count -ne 1){throw 'A live renderer must defer native teardown immediately'}
  'Chrome profile exit checks: 5 passed'
} finally {Remove-Item -LiteralPath $fixture -Recurse -Force}
