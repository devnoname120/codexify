# User-invoked launcher; does not change or create scheduled tasks.
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Net.Http
. (Join-Path $PSScriptRoot 'windows-start-local-core.ps1')
$applicationRoot=Join-Path $env:USERPROFILE '.codexify'
$nativeExe=Join-Path $applicationRoot 'bin\codexify.exe'
$configPath=Join-Path $applicationRoot 'codexify.config.json'
if(-not(Test-Path -LiteralPath $nativeExe -PathType Leaf) -or -not(Test-Path -LiteralPath $configPath -PathType Leaf)){throw 'Codexify installation/configuration was not found'}
$logDirectory=Join-Path $applicationRoot 'logs'
$null=New-Item -ItemType Directory -Path $logDirectory -Force
$launchId=[DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss')+'-'+[Guid]::NewGuid().ToString('N').Substring(0,6)
$handler=[Net.Http.HttpClientHandler]::new()
$handler.UseProxy=$false
$client=[Net.Http.HttpClient]::new($handler)
$client.Timeout=[TimeSpan]::FromSeconds(2)
function Read-LocalHealth {
  try {return ($client.GetStringAsync('http://127.0.0.1:3000/health').GetAwaiter().GetResult() | ConvertFrom-Json)}catch{return $null}
}
try {
  $health=Read-LocalHealth
  if($health.status -ne 'ok'){
    $existing=@(Get-CimInstance Win32_Process -Filter "Name='codexify.exe'" | Where-Object {$_.ExecutablePath -ieq $nativeExe})
    if(-not $existing.Count){
      Write-Host 'Starting Codexify...'
      $null=Start-Process -FilePath $nativeExe -ArgumentList @('service','run','--config',('"'+$configPath+'"')) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $logDirectory ($launchId+'-supervisor.stdout')) -RedirectStandardError (Join-Path $logDirectory ($launchId+'-supervisor.stderr'))
    }
    $deadline=[DateTime]::UtcNow.AddSeconds(60)
    do {$health=Read-LocalHealth; if($health.status -eq 'ok'){break}; Start-Sleep -Seconds 1}while([DateTime]::UtcNow -lt $deadline)
    if($health.status -ne 'ok'){throw ('Codexify did not become ready. Startup logs: '+$logDirectory)}
  }
  if(Repair-StartLocalStaleBrowserDescriptor -BrowserRoot (Join-Path $applicationRoot 'browser')){
    Write-Host 'Recovering the closed managed browser...'
  }
  Write-Host 'Opening the managed browser...'
  $browserStart=Invoke-StartLocalCli -Executable $nativeExe -Arguments @('--config',$configPath,'browser','start') -WorkingDirectory $applicationRoot
  [IO.File]::WriteAllText((Join-Path $logDirectory ($launchId+'-browser.stdout')),$browserStart.Output,[Text.UTF8Encoding]::new($false))
  [IO.File]::WriteAllText((Join-Path $logDirectory ($launchId+'-browser.stderr')),$browserStart.ErrorOutput,[Text.UTF8Encoding]::new($false))
  if($browserStart.ExitCode -ne 0){throw ('Browser startup needs attention. See '+(Join-Path $logDirectory ($launchId+'-browser.stderr')))}
  # Verify the saved owner bearer directly, bypassing system proxy discovery.
  # This URL contains a private fragment. Never print it or save it in logs.
  $dashboardUri=Get-StartLocalDashboardUriFromRuntime -RuntimePath (Join-Path $applicationRoot 'owner-chat.json')
  Write-Host 'Opening Codexify in your default browser...'
  Open-StartLocalDashboard $dashboardUri
  Write-Host 'Codexify started.'
}catch{
  Write-Host $_.Exception.Message -ForegroundColor Red
  exit 1
}finally{$client.Dispose();$handler.Dispose()}
