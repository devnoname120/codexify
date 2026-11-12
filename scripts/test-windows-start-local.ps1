# Run under both Windows PowerShell 5 and PowerShell 7. Never invokes the launcher
# or starts/stops the native service, supervisor, browser, or default browser.
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
. (Join-Path $PSScriptRoot 'windows-start-local-core.ps1')
function Assert-StartLocal([bool]$Condition,[string]$Message) {if(-not $Condition){throw $Message}}
$hostExe=(Get-Process -Id $PID).Path
$temporaryRoot=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')+'\'
$fixture=Join-Path $temporaryRoot ('codexify-start-local-test-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $fixture
$checks=New-Object 'Collections.Generic.List[string]'
try {
  $zeroScript='[Console]::Out.Write("fast-zero"); [Console]::Error.Write("diagnostic-zero"); exit 0'
  $zeroEncoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($zeroScript))
  $zero=Invoke-StartLocalCli -Executable $hostExe -Arguments @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',$zeroEncoded)
  Assert-StartLocal ($null -ne $zero.ExitCode -and $zero.ExitCode -eq 0 -and $zero.Output -eq 'fast-zero' -and $zero.ErrorOutput -eq 'diagnostic-zero') 'A successful child lost its exit code or captured streams.'
  $checks.Add('fast zero exit and stdout/stderr capture')
  $failureScript='[Console]::Out.Write("fast-failure"); [Console]::Error.Write("diagnostic-failure"); exit 7'
  $failureEncoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($failureScript))
  $failure=Invoke-StartLocalCli -Executable $hostExe -Arguments @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',$failureEncoded)
  Assert-StartLocal ($failure.ExitCode -eq 7 -and $failure.Output -eq 'fast-failure' -and $failure.ErrorOutput -eq 'diagnostic-failure') 'A failing child was reported as successful or lost diagnostics.'
  $checks.Add('nonzero native exit receipt')
  $argumentScript=Join-Path $fixture 'argument fixture.ps1'
  [IO.File]::WriteAllText($argumentScript,'param([Parameter(ValueFromRemainingArguments=$true)][string[]]$Values)' + "`r`n" + '[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write(($Values -join "`n")); exit 0',[Text.UTF8Encoding]::new($true))
  $arguments=@('path with spaces','quote"inside','C:\trailing space\','literal $() and ` characters',('Unicode-'+[char]0x03A9))
  $quoted=Invoke-StartLocalCli -Executable $hostExe -Arguments (@('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',$argumentScript)+$arguments)
  Assert-StartLocal ($quoted.ExitCode -eq 0 -and $quoted.Output -ceq ($arguments -join "`n")) 'Windows arguments changed during native process launch.'
  $checks.Add('spaces, quotes, trailing slashes, literal shell characters and Unicode')
  $priorNoProxy=$env:NO_PROXY
  try {
    $env:NO_PROXY='existing-bypass.invalid'
    $environmentScript='[Console]::Write([Environment]::GetEnvironmentVariable("NO_PROXY")); exit 0'
    $environmentEncoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($environmentScript))
    $environment=Invoke-StartLocalCli -Executable $hostExe -Arguments @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',$environmentEncoded)
    $bypasses=$environment.Output -split ','
    Assert-StartLocal ($environment.ExitCode -eq 0 -and 'existing-bypass.invalid' -in $bypasses -and '127.0.0.1' -in $bypasses -and 'localhost' -in $bypasses -and '::1' -in $bypasses) 'Existing bypasses or explicit loopback bypasses were lost.'
  } finally {$env:NO_PROXY=$priorNoProxy}
  $checks.Add('child proxy bypass preserves existing entries and includes loopback endpoints')
  $waitScript='Start-Sleep -Seconds 10; exit 0'
  $waitEncoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($waitScript))
  $watch=[Diagnostics.Stopwatch]::StartNew()
  $timedOut=$false
  try {$null=Invoke-StartLocalCli -Executable $hostExe -Arguments @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',$waitEncoded) -TimeoutMilliseconds 200}
  catch {$timedOut=$_.Exception.Message -like '*timed out*'}
  Assert-StartLocal ($timedOut -and $watch.Elapsed.TotalSeconds -lt 4) 'Native command timeout was not bounded.'
  $checks.Add('bounded transient command timeout')
  foreach($invalid in @('https://127.0.0.1:3120/','http://example.com/','http://localhost.evil/','http://user@localhost/','file:///C:/private',"http://localhost/`nprivate")){
    $rejected=$false
    try {$null=Get-StartLocalDashboardUri $invalid} catch {$rejected=$true}
    Assert-StartLocal $rejected 'An unsafe dashboard address was accepted.'
  }
  $privateUri=Get-StartLocalDashboardUri ('http://127.0.0.1:3120/#codexify_'+('a'*43))
  Assert-StartLocal ($privateUri.IsLoopback -and $privateUri.Scheme -eq 'http') 'A local dashboard address was rejected.'
  $checks.Add('local HTTP dashboard validation without token output')
  $runtimePath=Join-Path $fixture 'owner-chat.json'
  $fixtureToken='codexify_'+('a'*43)
  $originalDashboardProbe=(Get-Command Read-StartLocalDashboardResponse).ScriptBlock
  try {
    $script:dashboardProbeCount=0
    $script:dashboardResponse='{"configured":true,"connectorName":"Fixture","canSave":false}'
    function Read-StartLocalDashboardResponse([int]$Port,[string]$Token,[int]$TimeoutMilliseconds=5000) {
      Assert-StartLocal ($Port -eq 3120 -and $Token -ceq $fixtureToken -and $TimeoutMilliseconds -eq 5000) 'Runtime credentials or timeout were not passed to local verification.'
      $script:dashboardProbeCount++
      return $script:dashboardResponse
    }
    [IO.File]::WriteAllText($runtimePath,(@{port=3120;token=$fixtureToken}|ConvertTo-Json))
    $verified=Get-StartLocalDashboardUriFromRuntime $runtimePath
    Assert-StartLocal ($verified.IsLoopback -and $verified.Port -eq 3120 -and $script:dashboardProbeCount -eq 1) 'Valid runtime verification failed.'
    $script:dashboardResponse='{"configured":false,"connectorName":null,"canSave":true}'
    $null=Get-StartLocalDashboardUriFromRuntime $runtimePath
    foreach($badRuntime in @(
      @{port=0;token=$fixtureToken},@{port=65536;token=$fixtureToken},@{port='3120';token=$fixtureToken},
      @{port=3120.5;token=$fixtureToken},@{port=3120;token='bad-token'}
    )){
      [IO.File]::WriteAllText($runtimePath,($badRuntime|ConvertTo-Json))
      $before=$script:dashboardProbeCount
      $rejected=$false
      try {$null=Get-StartLocalDashboardUriFromRuntime $runtimePath} catch {$rejected=$true}
      Assert-StartLocal ($rejected -and $script:dashboardProbeCount -eq $before) 'Invalid runtime credentials reached the HTTP verifier.'
    }
    [IO.File]::WriteAllText($runtimePath,(@{port=3120;token=$fixtureToken}|ConvertTo-Json))
    foreach($badResponse in @('{"configured":"true","connectorName":null,"canSave":true}','{"configured":true,"connectorName":4,"canSave":true}','{"configured":true,"connectorName":null}','null','invalid-json')){
      $script:dashboardResponse=$badResponse
      $rejected=$false
      try {$null=Get-StartLocalDashboardUriFromRuntime $runtimePath} catch {$rejected=$true}
      Assert-StartLocal $rejected 'An invalid dashboard response was accepted.'
    }
  } finally {Set-Item -Path Function:Read-StartLocalDashboardResponse -Value $originalDashboardProbe}
  $checks.Add('runtime credentials validated before HTTP and tiny owner-connector schema checked')
  $applicationRoot=Join-Path $env:USERPROFILE '.codexify'
  $nativeExe=Join-Path $applicationRoot 'bin\codexify.exe'
  $configPath=Join-Path $applicationRoot 'codexify.config.json'
  $nativeStatusSamples=New-Object 'Collections.Generic.List[object]'
  for($sample=1;$sample -le 3;$sample++){
    $status=Invoke-StartLocalCli -Executable $nativeExe -Arguments @('--config',$configPath,'browser','status','--json') -WorkingDirectory $applicationRoot
    $state=$null
    $detail=''
    try {$statusDocument=$status.Output|ConvertFrom-Json; $state=$statusDocument.state; $detail=[string]$statusDocument.detail} catch {}
    $safeError=($status.ErrorOutput+' '+$detail).Trim() -replace 'codexify_[A-Za-z0-9_-]+','[private token]' -replace '(?i)Bearer\s+\S+','Bearer [redacted]' -replace '(https?://[^\s#]+)#[^\s]+','$1#[redacted]'
    if($safeError.Length -gt 600){$safeError=$safeError.Substring(0,600)}
    $expectedExit=if($state -in @('ready','busy')){0}elseif($state -eq 'stopped'){3}elseif($state -in @('disabled','not_installed')){4}elseif($state -in @('incompatible','authentication_required','account_changed','unavailable')){1}else{-1}
    $nativeStatusSamples.Add([pscustomobject]@{sample=$sample;exitCode=$status.ExitCode;state=$state;expectedExitCode=$expectedExit;error=$safeError})
  }
  $nativeStatusSamples.ToArray() | ConvertTo-Json -Depth 3 | Write-Output
  Assert-StartLocal (@($nativeStatusSamples|Where-Object {$null -eq $_.exitCode -or $_.expectedExitCode -lt 0 -or $_.exitCode -ne $_.expectedExitCode}).Count -eq 0) 'A native status receipt disagreed with its JSON state; all diagnostics are recorded above.'
  $checks.Add('three independent installed status diagnostics preserve JSON and matching exit receipts, including genuine unavailability')
  $liveRuntimePath=Join-Path $applicationRoot 'owner-chat.json'
  $dashboard=Get-StartLocalDashboardUriFromRuntime $liveRuntimePath
  Assert-StartLocal $dashboard.IsLoopback 'Verified live dashboard was not loopback.'
  $liveRuntime=Get-Content -LiteralPath $liveRuntimePath -Raw|ConvertFrom-Json
  [IO.File]::WriteAllText($runtimePath,(@{port=$liveRuntime.port;token=$fixtureToken}|ConvertTo-Json))
  $authRejected=$false
  try {$null=Get-StartLocalDashboardUriFromRuntime $runtimePath}
  catch {$authRejected=$_.Exception.Message -notmatch 'codexify_[A-Za-z0-9_-]+'}
  Assert-StartLocal $authRejected 'An incorrect bearer was accepted or exposed in an error.'
  $checks.Add('real proxy-free owner HTTP verification and rejected incorrect bearer; browser not opened')
  [pscustomobject]@{ok=$true;shellVersion=$PSVersionTable.PSVersion.ToString();checks=$checks.ToArray()} | ConvertTo-Json -Depth 3
} finally {
  $resolvedFixture=[IO.Path]::GetFullPath($fixture)
  if($resolvedFixture.StartsWith($temporaryRoot,[StringComparison]::OrdinalIgnoreCase) -and (Split-Path -Leaf $resolvedFixture) -match '^codexify-start-local-test-[a-f0-9]{32}$'){
    Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
  }
}
