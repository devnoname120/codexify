$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-start-local-core.ps1')
$temporaryRoot=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')+'\'
$fixture=Join-Path $temporaryRoot ('codexify-stale-browser-test-'+[Guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $fixture
$path=Join-Path $fixture 'descriptor.json'
$listener=New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback,0)
$listener.Start()
$port=$listener.LocalEndpoint.Port
function Write-FixtureDescriptor([long]$BrowserPid,[bool]$Old=$true) {
  [IO.File]::WriteAllText($path,(@{version=1;kind='codexify-browser-companion';pid=$BrowserPid;endpoint=('http://127.0.0.1:'+$port)}|ConvertTo-Json))
  if($Old){(Get-Item -LiteralPath $path).LastWriteTimeUtc=[DateTime]::UtcNow.AddMinutes(-1)}
}
try {
  Write-FixtureDescriptor $PID
  if(Repair-StartLocalStaleBrowserDescriptor $fixture){throw 'A live process descriptor was moved.'}
  Write-FixtureDescriptor 4294967294
  if(Repair-StartLocalStaleBrowserDescriptor $fixture){throw 'A descriptor with a listening port was moved.'}
  $listener.Stop()
  Write-FixtureDescriptor 4294967294 $false
  if(Repair-StartLocalStaleBrowserDescriptor $fixture){throw 'A newly published descriptor was moved.'}
  Write-FixtureDescriptor 4294967294
  $original=[IO.File]::ReadAllText($path)
  if(-not(Repair-StartLocalStaleBrowserDescriptor $fixture)){throw 'A verified old dead-process descriptor was not recovered.'}
  $backups=@(Get-ChildItem -LiteralPath (Join-Path $fixture 'stale-descriptors') -File)
  if((Test-Path -LiteralPath $path) -or $backups.Count -ne 1 -or [IO.File]::ReadAllText($backups[0].FullName) -cne $original){throw 'Recovery did not preserve the descriptor exactly.'}
  [pscustomobject]@{ok=$true;shellVersion=$PSVersionTable.PSVersion.ToString();checks=@('live PID preserved','listening endpoint preserved','fresh descriptor preserved','dead PID and refused endpoint recovered','old receipt backed up byte-for-byte')}|ConvertTo-Json -Depth 3
} finally {
  $listener.Stop()
  $resolved=[IO.Path]::GetFullPath($fixture)
  if($resolved.StartsWith($temporaryRoot,[StringComparison]::OrdinalIgnoreCase) -and (Split-Path -Leaf $resolved) -match '^codexify-stale-browser-test-[a-f0-9]{32}$'){
    Remove-Item -LiteralPath $resolved -Recurse -Force
  }
}
