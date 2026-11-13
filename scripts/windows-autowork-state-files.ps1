function Get-AutoworkStateFiles([string]$Root) {
  # Test projects live beside real projects. The owner API is the authority
  # on which projects are actually registered; recursive scans include their
  # nested fixtures and expose fake active goals to recovery monitors.
  $owner = Get-Content -LiteralPath (Join-Path $Root 'owner-chat.json') -Raw | ConvertFrom-Json
  if ($owner.port -lt 1 -or $owner.port -gt 65535 -or -not $owner.token) {
    throw 'Invalid local owner descriptor'
  }
  Add-Type -AssemblyName System.Net.Http
  $handler = [Net.Http.HttpClientHandler]::new()
  $handler.UseProxy = $false
  $handler.AllowAutoRedirect = $false
  $client = [Net.Http.HttpClient]::new($handler)
  $client.Timeout = [TimeSpan]::FromSeconds(25)
  $client.DefaultRequestHeaders.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $owner.token)
  try {
    $reply = $client.GetAsync(('http://127.0.0.1:' + $owner.port + '/api/chats')).GetAwaiter().GetResult()
    try {
      if (-not $reply.IsSuccessStatusCode) { throw 'The owner chat inventory is unavailable; no filesystem guesses were used' }
      $inventory = $reply.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json
    } finally { $reply.Dispose() }
  } finally { $client.Dispose(); $handler.Dispose() }
  if (-not $inventory.PSObject.Properties['chats']) { throw 'Invalid owner chat inventory' }
  Select-AutoworkStateFiles $Root $inventory
}

function Select-AutoworkStateFiles([string]$Root, $Inventory) {
  $allowed = @{}
  foreach ($chat in $Inventory.chats) { $allowed[[string]$chat.id] = $true }
  $projectRoot = Join-Path $Root 'projects'
  foreach ($project in Get-ChildItem -LiteralPath $projectRoot -Directory) {
    if ($project.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
    # Only the native storage layout, never worktrees, backups or test trees.
    $chatRoot = Join-Path $project.FullName 'chats'
    if (-not (Test-Path -LiteralPath $chatRoot -PathType Container)) { continue }
    if ((Get-Item -LiteralPath $chatRoot).Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
    foreach ($chat in Get-ChildItem -LiteralPath $chatRoot -Directory) {
      if ($chat.Name -notmatch '^[a-f0-9]{64}$' -or ($chat.Attributes -band [IO.FileAttributes]::ReparsePoint)) { continue }
      $transcript = Join-Path $chat.FullName 'CHAT.md'
      # Native PathBuf keeps the slash in its default '.codexify/projects'
      # join. Match the API's exact path hash before accepting either spelling.
      $nativeTranscript = $Root.TrimEnd('\','/') + '/projects\' + $project.Name + '\chats\' + $chat.Name + '\CHAT.md'
      $sha = [Security.Cryptography.SHA256]::Create()
      $registered = $false
      try {
        foreach ($spelling in @($transcript, $nativeTranscript)) {
          $id = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($spelling)) | ForEach-Object { $_.ToString('x2') })
          if ($allowed.ContainsKey($id)) { $registered = $true; break }
        }
      }
      finally { $sha.Dispose() }
      if (-not $registered) { continue }
      $path = Join-Path $chat.FullName 'automation.json'
      if (Test-Path -LiteralPath $path -PathType Leaf) { Get-Item -LiteralPath $path }
    }
  }
}
