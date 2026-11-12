#Requires -Version 7.0
param([ValidateRange(1,8)][int]$Jobs=4)
$ErrorActionPreference='Stop'
$repo=Split-Path -Parent $PSScriptRoot
$ready=Join-Path $repo 'browser-companion/artifacts/ready-20261030'
$statePath=Join-Path $ready 'agent-build-state.json'
$stdout=Join-Path $ready 'agent-build.stdout.txt'
$stderr=Join-Path $ready 'agent-build.stderr.txt'
function Save-BuildState($Value){
  $temporary=$statePath+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
  [IO.File]::WriteAllText($temporary,($Value|ConvertTo-Json -Depth 4))
  [IO.File]::Move($temporary,$statePath,$true)
}
try {
  # Bound parallelism instead of lowering Windows process priority. BelowNormal
  # delayed warm Cargo startup for minutes even with spare CPU; Normal started
  # the compiler within two seconds. Never elevate above ordinary development.
  [Diagnostics.Process]::GetCurrentProcess().PriorityClass='Normal'
  $env:CARGO_TARGET_DIR=Join-Path $repo 'target/optimized'
  $cargo=Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe'
  $started=[datetime]::UtcNow.ToString('o')
  $process=Start-Process -FilePath $cargo -ArgumentList @('build','--locked','--offline','--release','--bin','codexify','--jobs',"$Jobs") `
    -WorkingDirectory $repo -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
  $process.PriorityClass='Normal'
  do {
    $process.Refresh()
    Save-BuildState @{state='building';pid=$process.Id;startedAtUtc=$started;checkedAtUtc=[datetime]::UtcNow.ToString('o');jobs=$Jobs;priority='Normal';cpuSeconds=$process.CPU;stdoutBytes=(Get-Item -LiteralPath $stdout).Length;stderrBytes=(Get-Item -LiteralPath $stderr).Length}
    $null=$process.WaitForExit(15000)
  }while(-not $process.HasExited)
  if($process.ExitCode -ne 0){throw "Release build exited $($process.ExitCode); see agent-build.stderr.txt"}
  # Compilation can yield indefinitely; final verification must finish rather
  # than timing out a ready executable under background scheduling pressure.
  [Diagnostics.Process]::GetCurrentProcess().PriorityClass='Normal'
  Save-BuildState @{state='building';phase='verifying';pid=$PID;startedAtUtc=$started;checkedAtUtc=[datetime]::UtcNow.ToString('o')}
  $built=Join-Path $repo 'target/optimized/release/codexify.exe'
  $candidate=Join-Path $ready 'agent-native.exe'
  Copy-Item -LiteralPath $built -Destination $candidate -Force
  $sha=(Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash.ToLowerInvariant()
  if($sha -ne (Get-FileHash -LiteralPath $built -Algorithm SHA256).Hash.ToLowerInvariant()){throw 'Native candidate hash mismatch'}
  $probe=Start-Process -FilePath $candidate -ArgumentList '--help' -WorkingDirectory $repo -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput (Join-Path $ready 'agent-build-probe.stdout.txt') -RedirectStandardError (Join-Path $ready 'agent-build-probe.stderr.txt')
  $probe.PriorityClass='Normal'
  if(-not $probe.WaitForExit(45000)){throw 'Native help probe did not finish; installed binary preserved'}
  if($probe.ExitCode -ne 0){throw 'Native help probe failed; installed binary preserved'}
  $receiptPath=Join-Path $ready 'agent-update.json'
  $receipt=Get-Content -LiteralPath $receiptPath -Raw|ConvertFrom-Json
  $native=Join-Path $env:USERPROFILE '.codexify/bin/codexify.exe'
  if((Get-FileHash -LiteralPath $native -Algorithm SHA256).Hash -ine $receipt.nativeSha256){throw 'Installed native file changed independently; candidate preserved for review'}
  $backupRoot=Join-Path $env:USERPROFILE '.codexify/backups/agent-reliability'
  $null=New-Item -ItemType Directory -Path $backupRoot -Force
  if((Get-Item -LiteralPath $backupRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Backup destination must be a regular directory'}
  $pending=$native+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
  Copy-Item -LiteralPath $candidate -Destination $pending
  # The running server keeps its existing image. Activation uses the idle
  # guard and existing supervisor; this script never stops any process.
  [IO.File]::Replace($pending,$native,(Join-Path $backupRoot ('native-'+[Guid]::NewGuid().ToString('N')+'.exe')),$true)
  $receipt.nativeSha256=$sha
  $temporary=$receiptPath+'.'+[Guid]::NewGuid().ToString('N')+'.tmp'
  [IO.File]::WriteAllText($temporary,($receipt|ConvertTo-Json))
  [IO.File]::Move($temporary,$receiptPath,$true)
  Save-BuildState @{state='staged';nativeSha256=$sha;candidate=$candidate;startedAtUtc=$started;finishedAtUtc=[datetime]::UtcNow.ToString('o');activation='pending_idle_guard'}
  @{state='staged';nativeSha256=$sha}|ConvertTo-Json
}catch{
  Save-BuildState @{state='failed';error=$_.Exception.Message;finishedAtUtc=[datetime]::UtcNow.ToString('o')}
  throw
}
