$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows-autowork-state-files.ps1')
$base = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
$root = Join-Path $base ('codexify-monitor-test-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $root
function Transcript-Id([string]$Path) {
  $sha = [Security.Cryptography.SHA256]::Create()
  try { -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Path)) | ForEach-Object { $_.ToString('x2') }) }
  finally { $sha.Dispose() }
}
try {
  $key = 'a' * 64
  $real = Join-Path $root ('projects\registered\chats\' + $key)
  $fake = Join-Path $root ('projects\.tmp-old-fixture\chats\' + ('b' * 64))
  $nested = Join-Path $root ('projects\registered\fixtures\nested\chats\' + ('c' * 64))
  foreach ($dir in @($real,$fake,$nested)) {
    $null = New-Item -ItemType Directory -Path $dir -Force
    [IO.File]::WriteAllText((Join-Path $dir 'CHAT.md'), '# fixture')
    [IO.File]::WriteAllText((Join-Path $dir 'automation.json'), '{"goal":{"status":"active"}}')
  }
  $inventory = [pscustomobject]@{chats=@([pscustomobject]@{id=(Transcript-Id (Join-Path $real 'CHAT.md'))})}
  $files = @(Select-AutoworkStateFiles $root $inventory)
  if ($files.Count -ne 1 -or $files[0].FullName -ne (Join-Path $real 'automation.json')) { throw 'Unregistered or nested test state entered the monitor' }
  $nativePath = $root + '/projects\registered\chats\' + $key + '\CHAT.md'
  $inventory.chats[0].id = Transcript-Id $nativePath
  if (@(Select-AutoworkStateFiles $root $inventory).Count -ne 1) { throw 'Native mixed-separator owner ID was lost' }
  $inventory.chats = @()
  if (@(Select-AutoworkStateFiles $root $inventory).Count -ne 0) { throw 'Empty live inventory fell back to historical goals' }
  Write-Output 'PASS: registered chats, fake active fixtures, nested fixtures, native path spelling, empty inventory'
} finally {
  $resolved = [IO.Path]::GetFullPath($root)
  if (-not $resolved.StartsWith($base,[StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolved) -notlike 'codexify-monitor-test-*') { throw 'Unsafe test cleanup path' }
  Remove-Item -LiteralPath $resolved -Recurse -Force
}
