# Shared by the native Windows launcher and its smoke tests. Windows PowerShell 5.1 compatible.
Set-StrictMode -Version Latest

function Get-LauncherProperty($Object, [string]$Name, $Default = $null) {
    if ($null -ne $Object -and $null -ne $Object.PSObject.Properties[$Name]) { return $Object.$Name }
    return $Default
}

function New-LauncherContext([string]$HomeDirectory = (Join-Path $env:USERPROFILE '.codexify')) {
    $root = [IO.Path]::GetFullPath($HomeDirectory)
    [pscustomobject]@{
        HomeDirectory = $root
        Executable = Join-Path $root 'bin\codexify.exe'
        ConfigPath = Join-Path $root 'codexify.config.json'
        RuntimePath = Join-Path $root 'owner-chat.json'
        LauncherDirectory = Join-Path $root 'launcher'
    }
}

function Get-LauncherSettings($Context) {
    if (-not (Test-Path -LiteralPath $Context.Executable -PathType Leaf)) {
        throw "Codexify is missing from $($Context.Executable). Install Codexify first."
    }
    if (-not (Test-Path -LiteralPath $Context.ConfigPath -PathType Leaf)) {
        throw 'No saved configuration found. Run codexify quickstart first.'
    }
    try { $config = Get-Content -LiteralPath $Context.ConfigPath -Raw | ConvertFrom-Json }
    catch { throw 'The saved Codexify configuration is not valid JSON.' }
    $chat = Get-LauncherProperty $config 'agentChat'
    $automation = Get-LauncherProperty $chat 'automation'
    $tunnels = @(Get-LauncherProperty $config 'openaiTunnels' @() | Where-Object { $null -ne $_ })
    $single = Get-LauncherProperty $config 'openaiTunnel'
    if ($null -ne $single) { $tunnels += $single }
    $port = Get-LauncherProperty $config 'port' 3000
    if ($null -eq $port) { $port = 3000 }
    [pscustomobject]@{
        MainPort = [int]$port
        ChatEnabled = (Get-LauncherProperty $chat 'enabled' $true) -ne $false
        BrowserEnabled = (Get-LauncherProperty $automation 'enabled' $false) -eq $true
        TunnelCount = $tunnels.Count
        WorkDirectory = [string](Get-LauncherProperty $config 'workDir' '')
        ConnectorName = [string](Get-LauncherProperty $automation 'connectorName' '')
    }
}

function Protect-LauncherText([string]$Value) {
    $Value = $Value -replace '(?i)((?:api[_-]?key|token|secret|password)\s*[:=]\s*)[^\s,;]+', '$1[redacted]'
    $Value = $Value -replace 'codexify_[A-Za-z0-9_-]+', '[private token]'
    $Value = $Value -replace 'sk-[A-Za-z0-9_-]+', '[private key]'
    $Value = $Value -replace '(?i)(Bearer\s+)\S+', '$1[redacted]'
    return $Value
}

function ConvertTo-LauncherArgument([string]$Value) {
    # Windows argv quoting, with no cmd.exe or PowerShell evaluation.
    return '"' + ($Value -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

function Invoke-LauncherCli($Context, [string[]]$Arguments, [int]$TimeoutSeconds = 30, [switch]$AllowFailure) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $Context.Executable
    $info.Arguments = (@('--config', $Context.ConfigPath) + $Arguments | ForEach-Object { ConvertTo-LauncherArgument $_ }) -join ' '
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [Text.Encoding]::UTF8
    $info.WorkingDirectory = $Context.HomeDirectory
    $info.EnvironmentVariables['NO_COLOR'] = '1'
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $info
    try {
        if (-not $process.Start()) { throw 'Could not launch Codexify.' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            throw 'Codexify command timed out. Check service logs before retrying.'
        }
        # A persistent browser descendant can inherit a copy of the CLI's pipe
        # handles. Parent exit is the command receipt; do not wait forever for
        # every descendant to close those handles before checking live health.
        $output = if ($stdout.Wait(1500)) { $stdout.GetAwaiter().GetResult().Trim() } else { '' }
        $errorText = if ($stderr.Wait(1500)) { $stderr.GetAwaiter().GetResult().Trim() } else { '' }
        if ($process.ExitCode -ne 0 -and -not $AllowFailure) {
            $safeError = Protect-LauncherText $errorText
            if ($safeError.Length -gt 600) { $safeError = $safeError.Substring(0, 600) }
            throw "Codexify command failed ($($process.ExitCode)). $safeError"
        }
        return [pscustomobject]@{ ExitCode = $process.ExitCode; Output = $output }
    } finally { $process.Dispose() }
}

function Get-LauncherJson($Context, [string[]]$Arguments) {
    $reply = Invoke-LauncherCli $Context $Arguments -AllowFailure
    try { return $reply.Output | ConvertFrom-Json }
    catch { throw 'Codexify returned an unreadable status. Check the installed version and service logs.' }
}

function Get-LauncherRuntime($Context) {
    try {
        $runtime = Get-Content -LiteralPath $Context.RuntimePath -Raw -ErrorAction Stop | ConvertFrom-Json
        $port = [int]$runtime.port
        if ($port -lt 1 -or $port -gt 65535 -or $runtime.token -cnotmatch '^codexify_[A-Za-z0-9_-]{43}$') { return $null }
        return $runtime
    } catch { return $null }
}

function Get-LauncherHttp([string]$Url, [string]$Token = '') {
    # Never follow redirects with the private owner token, and bypass machine proxy settings.
    $request = [Net.HttpWebRequest]::Create($Url)
    $request.Proxy = $null
    $request.AllowAutoRedirect = $false
    $request.Timeout = 2500
    $request.ReadWriteTimeout = 2500
    if ($Token) { $request.Headers['Authorization'] = 'Bearer ' + $Token }
    $response = $null
    $reader = $null
    try {
        try { $response = $request.GetResponse() }
        catch [Net.WebException] { $response = $_.Exception.Response; if ($null -eq $response) { return $null } }
        if ([int]$response.StatusCode -notin @(200, 503)) { return $null }
        $reader = New-Object IO.StreamReader($response.GetResponseStream())
        return $reader.ReadToEnd() | ConvertFrom-Json
    } catch { return $null }
    finally { if ($reader) { $reader.Dispose() }; if ($response) { $response.Dispose() } }
}

function Get-LauncherConnections($Context, $Settings) {
    $health = Get-LauncherHttp "http://127.0.0.1:$($Settings.MainPort)/health"
    $runtime = Get-LauncherRuntime $Context
    $owner = $null
    if ($runtime) { $owner = Get-LauncherHttp "http://127.0.0.1:$($runtime.port)/api/health" $runtime.token }
    $serverReady = (Get-LauncherProperty $health 'status') -eq 'ok'
    $count = Get-LauncherProperty $health 'tunnelsConfigured'
    $ready = Get-LauncherProperty $health 'tunnelsReady'
    # Older single-tunnel releases expose readiness through status, without counts.
    $tunnelReady = $Settings.TunnelCount -gt 0 -and $serverReady
    if ($null -ne $count) { $tunnelReady = $tunnelReady -and [int]$count -gt 0 -and [int]$ready -ge [int]$count }
    [pscustomobject]@{
        ServerReady = $serverReady
        ChatReady = $null -ne $owner -and (Get-LauncherProperty $owner 'status') -in @('ok', 'starting')
        ChatPort = if ($runtime) { [int]$runtime.port } else { $null }
        TunnelReady = $tunnelReady
        TunnelsReady = $ready
        TunnelsConfigured = if ($null -ne $count) { [int]$count } else { $Settings.TunnelCount }
    }
}

function Get-LauncherSnapshot($Context) {
    $settings = Get-LauncherSettings $Context
    $service = Get-LauncherJson $Context @('service', 'status', '--json')
    $browser = Get-LauncherJson $Context @('browser', 'status', '--json')
    $connections = Get-LauncherConnections $Context $settings
    $browserReady = -not $settings.BrowserEnabled -or (Get-LauncherProperty $browser 'state') -in @('ready', 'busy')
    [pscustomobject]@{
        Settings = $settings
        Service = $service
        BrowserState = [string](Get-LauncherProperty $browser 'state' 'unavailable')
        BrowserReady = $browserReady
        Connections = $connections
        Ready = $connections.ChatReady -and $connections.TunnelReady -and $browserReady
        CheckedAt = [DateTime]::Now.ToString('HH:mm:ss')
    }
}

function Set-LauncherProgress($ProgressState, [string]$Message) {
    if ($null -ne $ProgressState) { $ProgressState.Message = $Message }
}

function Invoke-LauncherAction($Context, [ValidateSet('Start', 'Stop', 'Refresh', 'Login', 'Reveal')][string]$Action, $ProgressState = $null) {
    $problem = $null
    $snapshot = $null
    try {
        $settings = Get-LauncherSettings $Context
        switch ($Action) {
            'Start' {
                Set-LauncherProgress $ProgressState 'Reading saved configuration...'
                if (-not $settings.ChatEnabled) { throw 'Agent chat is disabled in the saved configuration. Enable agentChat.enabled first.' }
                if ($settings.TunnelCount -eq 0) { throw 'No saved OpenAI tunnel found. Run codexify quickstart to connect one first.' }
                $service = Get-LauncherJson $Context @('service', 'status', '--json')
                Set-LauncherProgress $ProgressState 'Starting the Codexify service and tunnel...'
                if (-not $service.installed) {
                    $null = Invoke-LauncherCli $Context @('service', 'install') -TimeoutSeconds 60
                } elseif (-not $service.running) {
                    $null = Invoke-LauncherCli $Context @('service', 'start')
                }
                if ($settings.BrowserEnabled) {
                    Set-LauncherProgress $ProgressState 'Restoring the saved ChatGPT browser session...'
                    $browser = Get-LauncherJson $Context @('browser', 'status', '--json')
                    if ($browser.state -notin @('ready', 'busy', 'authentication_required', 'account_changed')) {
                        $null = Invoke-LauncherCli $Context @('browser', 'start') -TimeoutSeconds 120
                    }
                }
                Set-LauncherProgress $ProgressState 'Waiting for the chat listener and tunnel connection...'
                $deadline = [DateTime]::UtcNow.AddSeconds(90)
                do {
                    $connections = Get-LauncherConnections $Context $settings
                    if ($connections.ChatReady -and $connections.TunnelReady) { break }
                    Start-Sleep -Milliseconds 1000
                } while ([DateTime]::UtcNow -lt $deadline)
                if (-not $connections.ChatReady -or -not $connections.TunnelReady) {
                    throw 'Startup is still incomplete. Check the connection cards and service logs; you can retry Start.'
                }
            }
            'Stop' {
                Set-LauncherProgress $ProgressState 'Stopping the service and its tunnel...'
                $service = Get-LauncherJson $Context @('service', 'status', '--json')
                if ($service.installed) { $null = Invoke-LauncherCli $Context @('service', 'stop') -TimeoutSeconds 60 }
                if ($settings.BrowserEnabled) {
                    Set-LauncherProgress $ProgressState 'Stopping the browser companion...'
                    $null = Invoke-LauncherCli $Context @('browser', 'stop') -TimeoutSeconds 60
                }
            }
            'Login' {
                Set-LauncherProgress $ProgressState 'Opening the saved ChatGPT sign-in window...'
                $null = Invoke-LauncherCli $Context @('browser', 'login') -TimeoutSeconds 120
            }
            'Reveal' {
                Set-LauncherProgress $ProgressState 'Showing the ChatGPT browser...'
                $null = Invoke-LauncherCli $Context @('browser', 'reveal') -TimeoutSeconds 120
            }
        }
        Set-LauncherProgress $ProgressState 'Checking service, chat, tunnel and browser...'
        $snapshot = Get-LauncherSnapshot $Context
        if ($Action -eq 'Start' -and -not $snapshot.BrowserReady) {
            if ($snapshot.BrowserState -in @('authentication_required', 'account_changed')) {
                throw 'Chat and tunnel are online. Use Sign in to restore the ChatGPT session.'
            }
            throw 'Chat and tunnel are online. The browser companion still needs attention; check its status or use Sign in.'
        }
    } catch {
        $problem = Protect-LauncherText $_.Exception.Message
        if ($null -eq $snapshot) { try { $snapshot = Get-LauncherSnapshot $Context } catch {} }
    }
    [pscustomobject]@{ Action = $Action; Snapshot = $snapshot; Error = $problem }
}

function Open-LauncherChat($Context) {
    $runtime = Get-LauncherRuntime $Context
    if (-not $runtime) { throw 'No private chat listener was found. Start Codexify first.' }
    $health = Get-LauncherHttp "http://127.0.0.1:$($runtime.port)/api/health" $runtime.token
    if ($null -eq $health) { throw 'The chat listener is offline. Start Codexify first.' }
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = "http://127.0.0.1:$($runtime.port)/#$($runtime.token)"
    $info.UseShellExecute = $true
    $null = [Diagnostics.Process]::Start($info)
}

function Get-LauncherShortcutArguments([string]$ScriptPath) {
    return '-NoLogo -NoProfile -STA -ExecutionPolicy Bypass -WindowStyle Hidden -File ' + (ConvertTo-LauncherArgument $ScriptPath)
}

function Set-LauncherShortcut([string]$ShortcutPath, [string]$ScriptPath, [string]$ExecutablePath) {
    $shell = New-Object -ComObject WScript.Shell
    try {
        $link = $shell.CreateShortcut($ShortcutPath)
        $link.TargetPath = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $link.Arguments = Get-LauncherShortcutArguments $ScriptPath
        $link.WorkingDirectory = Split-Path -Parent $ScriptPath
        $link.Description = 'Start Codexify chat, OpenAI tunnel and the saved ChatGPT browser session'
        $link.IconLocation = "$ExecutablePath,0"
        $link.WindowStyle = 7
        $link.Save()
    } finally { $null = [Runtime.InteropServices.Marshal]::ReleaseComObject($shell) }
}
