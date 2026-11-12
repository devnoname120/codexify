param(
  [string]$Root = (Join-Path $env:USERPROFILE '.codexify'),
  [string[]]$ChatPrefix = @(),
  [string[]]$FollowAgentsOfChatPrefix = @(),
  [int]$Port = 3000,
  [string]$SnapshotPath,
  [switch]$Compact
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
. (Join-Path $PSScriptRoot 'windows-autowork-state-files.ps1')

function Read-SharedJson([string]$Path) {
  # Monitoring must not prevent the service from atomically replacing state.
  $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read,
    ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
  $reader = [IO.StreamReader]::new($stream)
  try { return $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
}

$now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$inventoryAvailable=$true
$inventoryError=$null
try { $files=@(Get-AutoworkStateFiles $Root) }
catch {
  $files=@()
  $inventoryAvailable=$false
  $inventoryError=$_.Exception.Message
  if($inventoryError.Length -gt 1000){$inventoryError=$inventoryError.Substring(0,1000)}
}
$uninitializedAgents=@()
if($inventoryAvailable -and $FollowAgentsOfChatPrefix.Count){
  $byKey=@{};foreach($file in $files){$byKey[$file.Directory.Name]=$file}
  $pending=[Collections.Generic.Queue[string]]::new()
  foreach($key in $byKey.Keys){if(@($FollowAgentsOfChatPrefix|Where-Object {$key.StartsWith($_)}).Count){$pending.Enqueue($key)}}
  if(-not $pending.Count){throw 'The requested coordinating chat was not found; no chat state was changed'}
  $team=[Collections.Generic.HashSet[string]]::new()
  while($pending.Count){
    $key=$pending.Dequeue();if(-not $team.Add($key)){continue}
    if($team.Count -gt 512){throw 'Agent membership exceeds the bounded monitor traversal'}
    if(-not $byKey.ContainsKey($key)){$uninitializedAgents+=@($key);continue}
    $member=Read-SharedJson $byKey[$key].FullName
    foreach($child in $member.agents.children){if($child.taskKey -match '^[a-f0-9]{64}$'){$pending.Enqueue($child.taskKey)}}
  }
  $ChatPrefix=@($ChatPrefix)+@($team)
}
$agents = @($files | ForEach-Object {
  $file = $_
  if ($ChatPrefix.Count -and -not @($ChatPrefix | Where-Object { $file.Directory.Name.StartsWith($_) }).Count) { return }
  try {
    $state = Read-SharedJson $file.FullName
    $cursorPath = Join-Path $file.Directory.FullName 'cursor.json'
    $cursor = if (Test-Path -LiteralPath $cursorPath) { Read-SharedJson $cursorPath } else { $null }
    # Recent changes remain visible, including a new chat with no goal yet.
    # An expired pairing or a cancelled and paused retry cannot keep an old
    # chat in the default view. Explicit prefix and family views retain it.
    $recent = [Math]::Max([long]$cursor.lastAgentCallAtMs, [long]$state.action.updatedAtMs) -ge ($now - 900000)
    if (-not $ChatPrefix.Count -and -not $recent) {
      $cancelledRetry = $state.action.status -eq 'cancelled' -and $state.action.automaticRetryPaused
      $activeGoal = $state.goal.status -eq 'active' -and -not $cancelledRetry
      $pairingExpiry = $state.pairing.pending.expiresAtMs
      $openPairing = $state.pairing.creatingChat -and ($null -eq $pairingExpiry -or $pairingExpiry -ge $now)
      if ($cursor.agentStopRequested -or (-not $activeGoal -and -not $openPairing)) { return }
    }
    [pscustomobject]@{
      chat = $file.Directory.Name
      project = $file.Directory.Parent.Parent.Name
      goal = $state.goal.status
      action = $state.action.status
      actionKind = $state.action.kind
      pairingGeneration = $state.pairing.generation
      creatingChat = [bool]$state.pairing.creatingChat
      pairingExpiresAtMs = $state.pairing.pending.expiresAtMs
      lastCallAgeSeconds = if ($cursor.lastAgentCallAtMs) { [math]::Max(0, [math]::Round(($now - $cursor.lastAgentCallAtMs) / 1000)) } else { $null }
      toolCalls = $cursor.totalToolCalls
      toolEpoch = $cursor.toolCallEpoch
      toolSequence = $cursor.toolCallSequence
      stopRequested = $cursor.agentStopRequested
      busyUntilMs = @($cursor.agentIdleUntilMs,$cursor.agentPresenceUntilMs,$cursor.agentWaitingUntilMs,0 | ForEach-Object {[long]$_} | Measure-Object -Maximum).Maximum
      turnCount = $state.goal.turnCount
      sendAccepted = $state.action.sendAccepted
      error = $state.action.lastError
      actionUpdatedAtMs = $state.action.updatedAtMs
      goalRevision = $state.goal.revision
      actionGoalRevision = $state.action.goalRevision
      cooldownUntilMs = $state.action.cooldownUntilMs
      handoffReportPending = [bool]($state.turnRenewal.handoff -and -not $state.turnRenewal.handoff.reportComplete)
      handoffAgeSeconds = if ($state.turnRenewal.handoff.requestedAtMs) { [math]::Max(0, [math]::Round(($now - $state.turnRenewal.handoff.requestedAtMs) / 1000)) } else { $null }
      recoveryAttempts = $state.goalWake.recoveryAttempts
      automaticRetryAttempts = $state.action.automaticRetryAttempts
      accessRetryAttempts = $state.action.chatgptAccessRetryAttempts
      automaticRetryPaused = $state.action.automaticRetryPaused
    }
  } catch {
    [pscustomobject]@{chat=$file.Directory.Name; stateReadFailed=$true}
  }
})

$browser = try {
  $descriptor = Read-SharedJson (Join-Path $Root 'browser\descriptor.json')
  $endpoint = [uri]$descriptor.endpoint
  if ($endpoint.Scheme -ne 'http' -or $endpoint.Host -notin @('127.0.0.1', '[::1]', '::1', 'localhost')) {
    throw 'Browser descriptor is not a loopback endpoint'
  }
  Invoke-RestMethod -Uri ($descriptor.endpoint.TrimEnd('/') + '/v1/health') -TimeoutSec 15 `
    -Headers @{Authorization=('Bearer ' + $descriptor.token)} |
    Select-Object build,status,authenticated,activeActionId,@{Name='authenticationState';Expression={
      # The passive browser API returns busy/false while identity is warming.
      if ($_.status -eq 'authentication_required') { 'required' }
      elseif ($_.authenticated -is [bool] -and $_.authenticated) { 'verified' }
      elseif ($_.status -eq 'busy') { 'pending' }
      else { 'unknown' }
    }}
} catch { [pscustomobject]@{status='unreachable';authenticationState='unknown'} }
$server = try {
  $health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 5
  $health.status
} catch { 'unreachable' }
$exe = Join-Path $Root 'bin\codexify.exe'
$processes = @(Get-CimInstance Win32_Process -Filter "Name = 'codexify.exe'" |
  Where-Object {$_.ExecutablePath -ieq $exe})
$supervisors = @($processes | Where-Object {$_.CommandLine -match '\sservice run\s'})
$agentTeams=$null
if(-not $inventoryAvailable){
  $agentTeams=[pscustomobject]@{available=$false;discoveryComplete=$false;allStatusesAvailable=$false;teams=$null;error='Published agent client or Node.js is unavailable'}
  $helper=Join-Path $Root 'agents\client.mjs'
  if((Test-Path -LiteralPath $helper -PathType Leaf) -and (Get-Command node -ErrorAction SilentlyContinue)){
    try {
      $nativePreference=$PSNativeCommandUseErrorActionPreference
      try {
        # Partial team observations are valid JSON even when the helper exits 1.
        $PSNativeCommandUseErrorActionPreference=$false
        $reply=& node (Join-Path $PSScriptRoot 'windows-autowork-team-status.mjs') $helper
      } finally {$PSNativeCommandUseErrorActionPreference=$nativePreference}
      $agentTeams=$reply|ConvertFrom-Json
      if(-not $agentTeams.PSObject.Properties['discoveryComplete']){throw 'Invalid published team observation'}
    } catch {
      $agentTeams=[pscustomobject]@{available=$false;discoveryComplete=$false;allStatusesAvailable=$false;teams=$null;error=$_.Exception.Message}
    }
  }
}
$snapshot = [pscustomobject]@{
  atUtc = [DateTime]::UtcNow.ToString('o')
  inventoryAvailable = $inventoryAvailable
  inventoryError = $inventoryError
  server = $server
  supervisorCount = $supervisors.Count
  freeDiskGiB = [math]::Round(([IO.DriveInfo]::new([IO.Path]::GetPathRoot($Root))).AvailableFreeSpace / 1GB, 2)
  browser = $browser
  # Keep known empty/single collections as arrays; null means unknown inventory.
  uninitializedAgents = if($inventoryAvailable){,@($uninitializedAgents)}else{$null}
  agents = if($inventoryAvailable){,@($agents)}else{$null}
  # Independent published teams do not make unknown all-chat inventory complete.
  agentTeams = $agentTeams
}
$json = $snapshot | ConvertTo-Json -Depth 9
if ($SnapshotPath) { [IO.File]::WriteAllText([IO.Path]::GetFullPath($SnapshotPath), $json) }
if ($Compact) {
  # Full ownership/proof details are retained in the saved snapshot above.
  foreach($team in $agentTeams.teams){
    foreach($worker in $team.workers){
      $worker.PSObject.Properties.Remove('scope')
      $worker.PSObject.Properties.Remove('blockedReason')
    }
  }
  [pscustomobject]@{
    atUtc=$snapshot.atUtc;server=$server;supervisors=$supervisors.Count;freeGiB=$snapshot.freeDiskGiB
    inventoryAvailable=$inventoryAvailable;inventoryError=$inventoryError
    browser=$browser.status;authenticated=$browser.authenticated;actionId=$browser.activeActionId
    browserAuthentication=$browser.authenticationState
    uninitializedAgents=$snapshot.uninitializedAgents
    agentTeams=$agentTeams
    agents=if($inventoryAvailable){,@($agents|ForEach-Object{[pscustomobject]@{
      chat=$_.chat.Substring(0,[Math]::Min(8,$_.chat.Length));goal=$_.goal;action=$_.action
      actionKind=$_.actionKind;sendAccepted=$_.sendAccepted
      pairingGeneration=$_.pairingGeneration;creatingChat=$_.creatingChat;pairingExpiresAtMs=$_.pairingExpiresAtMs
      calls=$_.toolCalls;age=$_.lastCallAgeSeconds;busyUntilMs=$_.busyUntilMs
      stopped=$_.stopRequested;paused=$_.automaticRetryPaused;error=$_.error;stateReadFailed=[bool]$_.stateReadFailed
      goalRevision=$_.goalRevision;actionGoalRevision=$_.actionGoalRevision
      cooldownUntilMs=$_.cooldownUntilMs;automaticRetryAttempts=$_.automaticRetryAttempts
      accessRetryAttempts=$_.accessRetryAttempts;recoveryAttempts=$_.recoveryAttempts
      handoffReportPending=$_.handoffReportPending;handoffAgeSeconds=$_.handoffAgeSeconds
    }})}else{$null}
  }|ConvertTo-Json -Depth 9 -Compress
  if(-not $inventoryAvailable){exit 1}
  return
}
$json
if(-not $inventoryAvailable){exit 1}
