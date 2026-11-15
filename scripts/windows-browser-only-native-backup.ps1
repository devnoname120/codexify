# Browser-only deployments preserve the installed native file, including any
# read/image lease Windows has not released after the service exits.
function Backup-NativeForBrowserOnlyUpdate([string]$Executable,[string]$Candidate,[string]$BackupExecutable) {
  $ErrorActionPreference='Stop'
  $source=[IO.Path]::GetFullPath($Executable)
  $target=[IO.Path]::GetFullPath($BackupExecutable)
  if($source -ieq $target -or [IO.Path]::GetFullPath($Candidate) -ieq $target){throw 'Native backup must have its own path'}
  if(Test-Path -LiteralPath $target){throw 'Native backup slot already exists'}
  $originalHash=(Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
  if((Get-FileHash -LiteralPath $Candidate -Algorithm SHA256).Hash -ne $originalHash){throw 'Browser-only deployment requires an unchanged native executable'}
  Copy-Item -LiteralPath $source -Destination $target
  if((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne $originalHash){throw 'Native backup hash mismatch'}
  return $originalHash
}
