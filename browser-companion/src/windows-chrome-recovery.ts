import {access} from "node:fs/promises";
import {execFile} from "node:child_process";
import {promisify} from "node:util";

// Run only before launching a new private Chrome, after Electron has acquired
// its single-instance lock. Never select by executable name alone.
export const recoveryScript = String.raw`
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$profile=[IO.Path]::GetFullPath($env:CODEXIFY_RECOVERY_PROFILE).TrimEnd('\')
$expected=[IO.Path]::GetFullPath($env:CODEXIFY_RECOVERY_EXECUTABLE)
$runtime=$env:CODEXIFY_RECOVERY_RUNTIME
if($runtime){$runtime=[IO.Path]::GetFullPath($runtime).TrimEnd('\')+'\'}
$all=@(Get-CimInstance Win32_Process)
$parents=@{};foreach($item in $all){$parents[[int]$item.ProcessId]=$item}
$stopped=@()
foreach($item in $all){
  if($item.Name -ne 'chrome.exe' -or -not $item.ExecutablePath -or -not $item.CommandLine){continue}
  $path=[IO.Path]::GetFullPath($item.ExecutablePath)
  if($path -ine $expected -and (-not $runtime -or -not $path.StartsWith($runtime,[StringComparison]::OrdinalIgnoreCase))){continue}
  $parent=$parents[[int]$item.ParentProcessId]
  # A PID reused after Chrome was created is not its owner.
  if($parent -and $parent.CreationDate -le $item.CreationDate){continue}
  $profiles=[regex]::Matches($item.CommandLine,'(?:^|\s)(?:"--user-data-dir=([^"]+)"|--user-data-dir="([^"]+)"|--user-data-dir=([^\s"]+))(?=\s|$)')
  if($profiles.Count -ne 1){continue}
  $value=@($profiles[0].Groups[1].Value,$profiles[0].Groups[2].Value,$profiles[0].Groups[3].Value | Where-Object {$_})[0]
  try {$actual=[IO.Path]::GetFullPath($value).TrimEnd('\')}catch{continue}
  if($actual -ine $profile){continue}
  # Recheck creation time, command line, executable and the dead parent just
  # before termination. A recycled PID or newly attached owner is left alone.
  $current=Get-CimInstance Win32_Process -Filter ("ProcessId="+[int]$item.ProcessId)
  if(-not $current -or $current.CreationDate -ne $item.CreationDate -or $current.ExecutablePath -ine $item.ExecutablePath -or $current.CommandLine -cne $item.CommandLine){continue}
  $parent=Get-CimInstance Win32_Process -Filter ("ProcessId="+[int]$current.ParentProcessId)
  if($parent -and $parent.CreationDate -le $current.CreationDate){continue}
  $process=Get-Process -Id $current.ProcessId -ErrorAction SilentlyContinue
  if(-not $process){continue}
  try {
    $started=([DateTimeOffset]$process.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds()
    $expectedStarted=([DateTimeOffset]$current.CreationDate.ToUniversalTime()).ToUnixTimeMilliseconds()
    if($started -ne $expectedStarted){continue}
    # Termination is asynchronous on Windows. Keep the verified handle until
    # exit releases Chrome's profile singleton before starting its replacement.
    $process.Kill()
    if(-not $process.WaitForExit(5000)){throw 'Private Chrome did not exit before its recovery deadline'}
  } finally {$process.Dispose()}
  $stopped+=[int]$current.ProcessId
}
@{stopped=$stopped.Count}|ConvertTo-Json -Compress
`;

export async function recoverOrphanedWindowsChrome(options: {
  profilePath:string; executablePath:string; managedRuntimeRoot?:string;
}): Promise<number> {
  if (process.platform !== "win32") return 0;
  // Synthetic surfaces and missing installations have no browser to recover.
  if (!await access(options.executablePath).then(() => true, () => false)) return 0;
  const {stdout} = await promisify(execFile)("powershell.exe", [
    "-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand",
    Buffer.from(recoveryScript, "utf16le").toString("base64"),
  ], {
    windowsHide:true, timeout:15_000, maxBuffer:16 * 1024,
    env:{...process.env,
      CODEXIFY_RECOVERY_PROFILE:options.profilePath,
      CODEXIFY_RECOVERY_EXECUTABLE:options.executablePath,
      CODEXIFY_RECOVERY_RUNTIME:options.managedRuntimeRoot ?? "",
    },
  });
  const result:unknown = JSON.parse(stdout.trim());
  if (!result || typeof result !== "object" || !("stopped" in result)
    || !Number.isSafeInteger(result.stopped) || (result.stopped as number) < 0) {
    throw new Error("Invalid private Chrome recovery result");
  }
  return result.stopped as number;
}
