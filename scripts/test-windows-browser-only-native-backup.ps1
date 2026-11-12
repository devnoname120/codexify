$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-browser-only-native-backup.ps1')
$fixture=Join-Path 'D:/CodexifyBuild' ('browser-only-native-test-'+[Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$exe=Join-Path $fixture 'installed.exe'
$candidate=Join-Path $fixture 'candidate.exe'
$backup=Join-Path $fixture 'backup.exe'
$stage=Join-Path $fixture 'codexify.incoming-local.exe'
[IO.File]::WriteAllText($exe,'immutable installed native fixture')
Copy-Item -LiteralPath $exe -Destination $candidate
[IO.File]::WriteAllText($stage,'prior staging file must be untouched')
$stamp=[IO.File]::GetLastWriteTimeUtc($exe)
$lease=[IO.File]::Open($exe,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
try {
 $hash=Backup-NativeForBrowserOnlyUpdate -Executable $exe -Candidate $candidate -BackupExecutable $backup
 if((Get-FileHash -LiteralPath $exe).Hash -ne $hash -or (Get-FileHash -LiteralPath $backup).Hash -ne $hash){throw 'Hash preservation failed'}
 if([IO.File]::GetLastWriteTimeUtc($exe) -ne $stamp){throw 'Installed file was rewritten'}
 if([IO.File]::ReadAllText($stage) -ne 'prior staging file must be untouched'){throw 'Browser-only backup touched native staging'}
 $refused=$false;try{Backup-NativeForBrowserOnlyUpdate $exe $candidate $backup}catch{$refused=$true};if(-not $refused){throw 'Reused backup slot was accepted'}
 [IO.File]::WriteAllText($candidate,'different native candidate')
 $refused=$false;try{Backup-NativeForBrowserOnlyUpdate $exe $candidate (Join-Path $fixture 'wrong-backup.exe')}catch{$refused=$true};if(-not $refused){throw 'Different native candidate was accepted'}
 [ordered]@{ok=$true;lockedNativePreserved=$true;unchangedTimestamp=$true;existingStageUntouched=$true;backupHashVerified=$true;wrongCandidateRejected=$true;reusedSlotRejected=$true;fixture=$fixture} | ConvertTo-Json -Compress
} finally {$lease.Dispose()}
