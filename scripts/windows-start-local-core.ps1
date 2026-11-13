# Side-effect-free helpers shared with the Windows PowerShell 5/7 smoke tests.
function ConvertTo-StartLocalArgument([string]$Value) {
  # Quote Windows argv without evaluating arguments through another shell.
  return '"'+($Value -replace '(\\*)"','$1$1\"' -replace '(\\+)$','$1$1')+'"'
}
function Invoke-StartLocalCli(
  [string]$Executable,
  [string[]]$Arguments,
  [int]$TimeoutMilliseconds=60000,
  [string]$WorkingDirectory
) {
  $info=New-Object Diagnostics.ProcessStartInfo
  $info.FileName=$Executable
  $info.Arguments=($Arguments | ForEach-Object {ConvertTo-StartLocalArgument $_}) -join ' '
  $info.UseShellExecute=$false
  $info.CreateNoWindow=$true
  $info.RedirectStandardOutput=$true
  $info.RedirectStandardError=$true
  $info.StandardOutputEncoding=[Text.Encoding]::UTF8
  $info.StandardErrorEncoding=[Text.Encoding]::UTF8
  $info.EnvironmentVariables['NO_COLOR']='1'
  # Native browser CLI builds may still consult the system proxy. Keep existing
  # bypasses and explicitly exempt the local companion/owner endpoints.
  $bypasses=@($info.EnvironmentVariables['NO_PROXY'],$info.EnvironmentVariables['no_proxy']) `
    + @('127.0.0.1','localhost','::1')
  $noProxy=(@($bypasses | ForEach-Object {if($_){$_ -split ','}} | ForEach-Object {$_.Trim()} | Where-Object {$_} | Select-Object -Unique)) -join ','
  $info.EnvironmentVariables['NO_PROXY']=$noProxy
  $info.EnvironmentVariables['no_proxy']=$noProxy
  if($WorkingDirectory){$info.WorkingDirectory=$WorkingDirectory}
  $process=New-Object Diagnostics.Process
  $process.StartInfo=$info
  try {
    if(-not $process.Start()){throw 'Could not launch the Codexify command.'}
    $stdout=$process.StandardOutput.ReadToEndAsync()
    $stderr=$process.StandardError.ReadToEndAsync()
    if(-not $process.WaitForExit($TimeoutMilliseconds)){
      try {$process.Kill()} catch {}
      throw 'Codexify command timed out; check startup logs before retrying.'
    }
    # Start-Process -PassThru can lose a fast child's exit code on Windows
    # PowerShell 5. Keep our own native process handle until its receipt is read.
    $exitCode=$process.ExitCode
    if($null -eq $exitCode){throw 'Codexify command did not provide an exit receipt.'}
    # Browser descendants can inherit pipe handles. Bound their drain after the
    # CLI itself exits instead of waiting for a persistent browser to close.
    $output=if($stdout.Wait(1500)){$stdout.GetAwaiter().GetResult().Trim()}else{''}
    $errorOutput=if($stderr.Wait(1500)){$stderr.GetAwaiter().GetResult().Trim()}else{''}
    return [pscustomobject]@{ExitCode=[int]$exitCode;Output=$output;ErrorOutput=$errorOutput}
  } finally {$process.Dispose()}
}
function Get-StartLocalServerPort([string]$ConfigPath) {
  # Same rule as the WPF launcher: the saved top-level port, 3000 when absent.
  $config=Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
  $port=3000
  if($config.PSObject.Properties['port'] -and $null -ne $config.port){$port=$config.port}
  $value=0
  if(-not [int]::TryParse([string]$port,[ref]$value) -or $value -lt 1 -or $value -gt 65535){throw 'The saved Codexify port is not valid'}
  return $value
}
function Get-StartLocalDashboardUri([string]$Output) {
  $value=$Output.Trim()
  $uri=$null
  if($value -match '[\r\n]' -or -not [Uri]::TryCreate($value,[UriKind]::Absolute,[ref]$uri) `
    -or $uri.Scheme -ne 'http' -or $uri.DnsSafeHost -notin @('127.0.0.1','localhost','::1') `
    -or $uri.UserInfo -or $uri.Port -lt 1 -or $uri.Port -gt 65535){
    throw 'Codexify returned an invalid local dashboard address.'
  }
  return $uri
}
function Read-StartLocalDashboardResponse([int]$Port,[string]$Token,[int]$TimeoutMilliseconds=5000) {
  Add-Type -AssemblyName System.Net.Http
  $handler=New-Object Net.Http.HttpClientHandler
  $handler.UseProxy=$false
  $handler.AllowAutoRedirect=$false
  $client=New-Object Net.Http.HttpClient($handler)
  $request=$null
  $response=$null
  try {
    $client.Timeout=[TimeSpan]::FromMilliseconds($TimeoutMilliseconds)
    $client.MaxResponseContentBufferSize=32*1024
    # Verify the authenticated owner surface without enumerating chat history.
    $request=New-Object Net.Http.HttpRequestMessage([Net.Http.HttpMethod]::Get,('http://127.0.0.1:'+$Port+'/api/settings/connector'))
    $request.Headers.Authorization=New-Object Net.Http.Headers.AuthenticationHeaderValue('Bearer',$Token)
    $response=$client.SendAsync($request).GetAwaiter().GetResult()
    if(-not $response.IsSuccessStatusCode){throw 'Local dashboard authentication or readiness failed.'}
    return $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  } finally {
    if($response){$response.Dispose()}
    if($request){$request.Dispose()}
    $client.Dispose()
    $handler.Dispose()
  }
}
function Get-StartLocalDashboardUriFromRuntime([string]$RuntimePath,[int]$TimeoutMilliseconds=5000) {
  try {
    $file=Get-Item -LiteralPath $RuntimePath -Force -ErrorAction Stop
    if($file.PSIsContainer -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 `
      -or $file.Length -gt 4096){throw 'Invalid local dashboard runtime file.'}
    $runtime=Get-Content -LiteralPath $RuntimePath -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    if(-not ($runtime.port -is [int] -or $runtime.port -is [long]) -or $runtime.port -lt 1 -or $runtime.port -gt 65535 `
      -or $runtime.token -isnot [string] -or $runtime.token -cnotmatch '^codexify_[A-Za-z0-9_-]{43}$'){
      throw 'Invalid local dashboard runtime credentials.'
    }
    $body=Read-StartLocalDashboardResponse -Port $runtime.port -Token $runtime.token -TimeoutMilliseconds $TimeoutMilliseconds
    $document=$body | ConvertFrom-Json -ErrorAction Stop
    if($null -eq $document -or $null -eq $document.PSObject.Properties['configured'] `
      -or $document.PSObject.Properties['configured'].Value -isnot [bool] `
      -or $null -eq $document.PSObject.Properties['canSave'] `
      -or $document.PSObject.Properties['canSave'].Value -isnot [bool] `
      -or $null -eq $document.PSObject.Properties['connectorName'] `
      -or ($null -ne $document.connectorName -and $document.connectorName -isnot [string])){
      throw 'Invalid local dashboard response.'
    }
    return Get-StartLocalDashboardUri ('http://127.0.0.1:'+$runtime.port+'/#'+$runtime.token)
  } catch {
    # Never surface a private bearer, response body, URL fragment or proxy error.
    throw 'Codexify dashboard verification failed. Check that the local owner chat is ready.'
  }
}
function Repair-StartLocalStaleBrowserDescriptor([string]$BrowserRoot) {
  # An exited Windows process can still have an open process handle. The old
  # native check mistakes that object for a live browser. Preserve the receipt
  # only after OS enumeration AND a refused private port prove it is stale.
  $browserDirectory=[IO.Path]::GetFullPath($BrowserRoot).TrimEnd('\')
  $descriptorPath=Join-Path $browserDirectory 'descriptor.json'
  if(-not(Test-Path -LiteralPath $descriptorPath -PathType Leaf)){return $false}
  $file=Get-Item -LiteralPath $descriptorPath -Force
  if(($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $file.Length -gt 8192 `
    -or $file.LastWriteTimeUtc -gt [DateTime]::UtcNow.AddSeconds(-30)){return $false}
  try {$descriptor=Get-Content -LiteralPath $descriptorPath -Raw|ConvertFrom-Json -ErrorAction Stop} catch {return $false}
  if($descriptor.kind -ne 'codexify-browser-companion' -or $descriptor.version -ne 1 `
    -or -not($descriptor.pid -is [int] -or $descriptor.pid -is [long]) -or $descriptor.pid -lt 1){return $false}
  if(@([Diagnostics.Process]::GetProcesses() | Where-Object Id -EQ $descriptor.pid).Count){return $false}
  $endpoint=$null
  if(-not [Uri]::TryCreate([string]$descriptor.endpoint,[UriKind]::Absolute,[ref]$endpoint) `
    -or $endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1' -or $endpoint.Port -lt 1 `
    -or $endpoint.UserInfo -or $endpoint.AbsolutePath -ne '/' -or $endpoint.Query -or $endpoint.Fragment){return $false}
  $originalHash=(Get-FileHash -LiteralPath $descriptorPath -Algorithm SHA256).Hash
  $profilePath=(Join-Path $browserDirectory 'chrome-profile').Replace('/','\')
  $runtimePrefix=(Join-Path $browserDirectory 'runtime').TrimEnd('\')+'\'
  $managed=@(Get-CimInstance Win32_Process -Filter "Name='codexify-browser-companion.exe' OR Name='chrome.exe'" -ErrorAction Stop |
    Where-Object {($_.Name -eq 'codexify-browser-companion.exe' -and (-not $_.ExecutablePath -or ([string]$_.ExecutablePath).Replace('/','\').StartsWith($runtimePrefix,[StringComparison]::OrdinalIgnoreCase))) `
      -or ([string]$_.CommandLine).Replace('/','\').IndexOf($profilePath,[StringComparison]::OrdinalIgnoreCase) -ge 0})
  if($managed.Count){return $false}
  $socket=New-Object Net.Sockets.TcpClient
  $refused=$false
  try {
    $connection=$socket.ConnectAsync('127.0.0.1',$endpoint.Port)
    if(-not $connection.Wait(3000)){return $false}
    $null=$connection.GetAwaiter().GetResult()
  } catch {
    $cause=$_.Exception
    while($cause.InnerException){$cause=$cause.InnerException}
    $refused=$cause -is [Net.Sockets.SocketException] -and $cause.SocketErrorCode -eq [Net.Sockets.SocketError]::ConnectionRefused
  } finally {$socket.Dispose()}
  if(-not $refused){return $false}
  if(@([Diagnostics.Process]::GetProcesses() | Where-Object Id -EQ $descriptor.pid).Count `
    -or (Get-FileHash -LiteralPath $descriptorPath -Algorithm SHA256).Hash -ne $originalHash){return $false}
  $backupRoot=Join-Path $browserDirectory 'stale-descriptors'
  $null=New-Item -ItemType Directory -Path $backupRoot -Force
  $backupDirectory=Get-Item -LiteralPath $backupRoot -Force
  if(($backupDirectory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'Stale browser backup must be a regular directory.'}
  $backupPath=[IO.Path]::GetFullPath((Join-Path $backupRoot ('descriptor-'+[DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss')+'-'+[Guid]::NewGuid().ToString('N')+'.json')))
  if(-not $backupPath.StartsWith($browserDirectory+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Stale browser backup must stay inside the browser directory.'}
  Move-Item -LiteralPath $descriptorPath -Destination $backupPath -ErrorAction Stop
  return $true
}
function Open-StartLocalDashboard([Uri]$Uri) {
  try {
    $info=New-Object Diagnostics.ProcessStartInfo
    $info.FileName=$Uri.AbsoluteUri
    $info.UseShellExecute=$true
    $null=[Diagnostics.Process]::Start($info)
  } catch {
    # Browser launch errors may include the URL fragment containing its token.
    throw 'Could not open the Codexify dashboard in the default browser.'
  }
}
