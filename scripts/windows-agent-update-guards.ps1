# Pure process classification for the guarded updater. A yielded exec call can
# leave a real build running even when HTTP activeRequests is zero.
function Test-BrowserPoolUnowned {
  param([object]$Health)
  # A pool can expose a free slot while another page owns a Send. Its singular
  # activeActionId is then null, so the complete ownership list is required.
  # Every updater checks this before any browser shutdown, and again before it.
  if (-not $Health -or $Health.activeActionId) { return $false }
  if ($Health.PSObject.Properties['activeActionIds'] -and
      @($Health.activeActionIds | Where-Object { $_ }).Count) { return $false }
  if ($Health.PSObject.Properties['actionCapacity'] -and
      ($Health.actionCapacity -lt 1 -or
       -not $Health.PSObject.Properties['availableActionSlots'] -or
       $Health.availableActionSlots -ne $Health.actionCapacity)) { return $false }
  return $true
}

function Test-AgentUpdateBrowserIdle {
  param([object]$Health)
  if (-not $Health -or $Health.kind -ne 'codexify-browser-companion' -or
      $Health.status -ne 'idle' -or $Health.authenticated -ne $true -or
      -not $Health.accountFingerprint) { return $false }
  return Test-BrowserPoolUnowned $Health
}

function Test-AgentUpdateBrowserStopped {
  param([object]$Status,[object[]]$LiveProcesses)
  return $Status.state -eq 'stopped' -and $Status.running -eq $false -and @($LiveProcesses).Count -eq 0
}

function Test-AgentUpdateBrowserQuiescent {
  param([object]$Health,[object]$Diagnostics)
  if (-not (Test-AgentUpdateBrowserIdle $Health) -or -not $Diagnostics -or
      $Diagnostics.generationRunning -isnot [bool] -or $Diagnostics.generationRunning) { return $false }
  $pages = if ($Health.PSObject.Properties['actionCapacity']) { [int]$Health.actionCapacity } else { 1 }
  return $Diagnostics.observedActionPages -eq $pages
}

function Test-AgentUpdateProcessRunning {
  param([object]$Row)
  $process=Get-Process -Id ([int]$Row.ProcessId) -ErrorAction SilentlyContinue
  if(-not $process){return $false}
  try {
    # Windows can retain a terminated process object while an exec session still
    # owns its handle. Existence in CIM/OpenProcess is not evidence of work.
    if($process.WaitForExit(0)){return $false}
    if($Row.CreationDate -and
      ([DateTimeOffset]$process.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds() -ne
      ([DateTimeOffset]$Row.CreationDate.ToUniversalTime()).ToUnixTimeMilliseconds()){
      return $true # Identity raced: defer rather than risk unrelated work.
    }
    return $true
  } catch {return $true} # Unverifiable owned work must defer.
  finally {$process.Dispose()}
}

function Get-AgentUpdateWorkProcesses {
  param([object[]]$Processes,[uint32]$ServerId,[uint32]$BrowserId,[string]$AppRoot,[string]$CuaRuntimeRoot,[string]$SystemRoot)
  $browserPrefix=[IO.Path]::GetFullPath((Join-Path $AppRoot 'browser/runtime')).TrimEnd('\')+'\'
  $tunnelPrefix=[IO.Path]::GetFullPath((Join-Path $AppRoot 'openai-tunnel')).TrimEnd('\')+'\'
  $cuaPrefix=[IO.Path]::GetFullPath($CuaRuntimeRoot).TrimEnd('\')+'\'
  $consolePath=[IO.Path]::GetFullPath((Join-Path $SystemRoot 'System32/conhost.exe'))
  $seen=[Collections.Generic.HashSet[uint32]]::new()
  $privateBrowser=[Collections.Generic.HashSet[uint32]]::new()
  $frontier=@($ServerId)
  $busy=@()
  while($frontier.Count){
    $children=@($Processes|Where-Object {$_.ParentProcessId -in $frontier -and -not $seen.Contains([uint32]$_.ProcessId)})
    $frontier=@()
    foreach($row in $children){
      $id=[uint32]$row.ProcessId
      $null=$seen.Add($id)
      $frontier+=$id
      $exe=[string]$row.ExecutablePath
      $command=[string]$row.CommandLine
      if($privateBrowser.Contains([uint32]$row.ParentProcessId) -or
        ($id -eq $BrowserId -and $exe.StartsWith($browserPrefix,[StringComparison]::OrdinalIgnoreCase) -and $row.Name -ieq 'codexify-browser-companion.exe')){
        $null=$privateBrowser.Add($id);continue
      }
      if($row.Name -ieq 'conhost.exe' -and $exe -ieq $consolePath){continue}
      # Only the known idle infrastructure is exempt. Inspect its descendants:
      # a user command launched through Node must still defer an update.
      $cua=$exe.StartsWith($cuaPrefix,[StringComparison]::OrdinalIgnoreCase)
      if($cua -and $row.Name -ieq 'node.exe' -and $row.ParentProcessId -eq $ServerId -and
        $command -match '@oai[\\/]cua-repl[\\/]bin[\\/]cua-repl\.mjs'){continue}
      if($cua -and $row.Name -ieq 'node_repl.exe' -and $command.Trim().Trim('"') -ieq $exe){continue}
      if($row.ParentProcessId -eq $ServerId -and $row.Name -ieq 'tunnel-client-runtime.exe' -and
        $exe.StartsWith($tunnelPrefix,[StringComparison]::OrdinalIgnoreCase) -and $command -match '\srun\s+--control-plane\.tunnel-id\s'){continue}
      $busy+=$row
    }
  }
  return $busy
}
