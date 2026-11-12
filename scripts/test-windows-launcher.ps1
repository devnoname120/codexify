Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows-launcher-core.ps1')

function Assert-Launcher([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
$temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
$fixture = Join-Path $temporaryRoot ('codexify-launcher-test-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path (Join-Path $fixture 'bin') -Force
$context = New-LauncherContext $fixture
$checks = New-Object 'Collections.Generic.List[string]'
try {
    # A real subprocess checks quoting, both redirected pipes and bounded timeouts.
    $stubSource = @'
using System;
using System.Threading;
using System.Diagnostics;
using System.IO;
public static class LauncherStub {
    public static void Main(string[] args) {
        Console.OutputEncoding = new System.Text.UTF8Encoding(false);
        if (Array.IndexOf(args, "persistent-child") >= 0) { Thread.Sleep(20000); return; }
        int spawn = Array.IndexOf(args, "spawn-child");
        if (spawn >= 0) {
            var info = new ProcessStartInfo(Process.GetCurrentProcess().MainModule.FileName, "persistent-child");
            info.UseShellExecute = false; info.CreateNoWindow = true;
            var child = Process.Start(info);
            File.WriteAllText(args[spawn + 1], child.Id.ToString());
            Console.WriteLine("Parent command completed"); return;
        }
        if (Array.IndexOf(args, "sleep") >= 0) { Thread.Sleep(5000); return; }
        if (Array.IndexOf(args, "flood") >= 0) {
            Console.Error.Write(new string('e', 180000));
            Console.Write(new string('o', 180000)); return;
        }
        if (Array.IndexOf(args, "fail") >= 0) {
            Console.Error.Write("token=codexify_PRIVATE sk-PRIVATE Bearer private");
            Environment.Exit(1);
        }
        Console.Write(string.Join("\n", args));
    }
}
'@
    Add-Type -TypeDefinition $stubSource -OutputAssembly $context.Executable -OutputType ConsoleApplication
    [IO.File]::WriteAllText($context.ConfigPath, '{"agentChat":{"enabled":true,"automation":{"enabled":true}},"openaiTunnel":{"tunnelId":"fixture"}}')
    $settings = Get-LauncherSettings $context
    Assert-Launcher ($settings.MainPort -eq 3000 -and $settings.TunnelCount -eq 1 -and $settings.BrowserEnabled) 'Saved configuration defaults failed.'
    $checks.Add('saved configuration and default ports')
    $tricky = @('path with spaces', 'quote"inside', 'C:\trailing space\', 'literal $() and ` characters', ('Unicode-' + [char]0x03A9))
    $reply = Invoke-LauncherCli $context $tricky
    $decoded = $reply.Output -split '\r?\n'
    Assert-Launcher ($decoded.Count -eq ($tricky.Count + 2)) "Subprocess argument count changed: $($decoded.Count) decoded, $($tricky.Count + 2) expected."
    Assert-Launcher ($decoded[0] -eq '--config' -and $decoded[1] -eq $context.ConfigPath) 'Explicit saved config was not passed.'
    for ($i = 0; $i -lt $tricky.Count; $i++) { Assert-Launcher ($decoded[$i + 2] -ceq $tricky[$i]) "Subprocess argument $i changed." }
    $checks.Add('subprocess quoting, spaces, quotes, trailing slashes and Unicode')
    $reply = Invoke-LauncherCli $context @('flood')
    Assert-Launcher ($reply.Output.Length -eq 180000) 'Redirected process streams blocked or truncated.'
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $timedOut = $false
    try { $null = Invoke-LauncherCli $context @('sleep') -TimeoutSeconds 1 } catch { $timedOut = $_.Exception.Message -like '*timed out*' }
    Assert-Launcher ($timedOut -and $watch.Elapsed.TotalSeconds -lt 4) 'Subprocess timeout was not bounded.'
    $checks.Add('redirected pipe draining and command timeout')
    $childPidFile = Join-Path $fixture 'child-pid.txt'
    $watch.Restart()
    try {
        $null = Invoke-LauncherCli $context @('spawn-child', $childPidFile) -TimeoutSeconds 5
        Assert-Launcher ($watch.Elapsed.TotalSeconds -lt 6) 'A persistent descendant held the launcher command open after its parent exited.'
        $childId = [int][IO.File]::ReadAllText($childPidFile)
        $child = Get-Process -Id $childId -ErrorAction Stop
        Assert-Launcher (-not $child.HasExited) 'The launcher terminated its persistent child.'
    } finally {
        if (Test-Path -LiteralPath $childPidFile) {
            $childId = [int][IO.File]::ReadAllText($childPidFile)
            $child = Get-Process -Id $childId -ErrorAction SilentlyContinue
            if ($child -and $child.Path -ieq $context.Executable) { $child.Kill(); $null = $child.WaitForExit(3000) }
        }
    }
    $checks.Add('persistent browser descendants cannot hold command completion open')
    $failure = ''
    try { $null = Invoke-LauncherCli $context @('fail') } catch { $failure = $_.Exception.Message }
    Assert-Launcher ($failure -like '*failed*' -and $failure -cnotmatch 'PRIVATE|Bearer private') 'Command errors exposed private values.'
    $checks.Add('private token and key redaction')

    $privateToken = 'codexify_' + ('a' * 43)
    @{ port = 43120; token = $privateToken } | ConvertTo-Json | Set-Content -LiteralPath $context.RuntimePath
    Assert-Launcher ($null -ne (Get-LauncherRuntime $context)) 'Saved owner token was not accepted.'
    @{ port = 70000; token = $privateToken } | ConvertTo-Json | Set-Content -LiteralPath $context.RuntimePath
    Assert-Launcher ($null -eq (Get-LauncherRuntime $context)) 'Invalid runtime port was accepted.'
    @{ port = 43120; token = $privateToken } | ConvertTo-Json | Set-Content -LiteralPath $context.RuntimePath

    $originalHttp = (Get-Command Get-LauncherHttp).ScriptBlock
    $script:fakeHealth = [pscustomobject]@{ status = 'ok'; tunnelsReady = 1; tunnelsConfigured = 2 }
    function Get-LauncherHttp([string]$Url, [string]$Token = '') {
        if ($Url -like '*/api/health') {
            Assert-Launcher ($Token -ceq $privateToken) 'Owner health request lost its private bearer.'
            return [pscustomobject]@{ status = 'ok' }
        }
        return $script:fakeHealth
    }
    $connection = Get-LauncherConnections $context $settings
    Assert-Launcher ($connection.ChatReady -and -not $connection.TunnelReady) 'Partial tunnel health was reported as fully connected.'
    $script:fakeHealth.tunnelsReady = 2
    Assert-Launcher ((Get-LauncherConnections $context $settings).TunnelReady) 'All-ready tunnel health was rejected.'
    $script:fakeHealth = [pscustomobject]@{ status = 'ok' }
    Assert-Launcher ((Get-LauncherConnections $context $settings).TunnelReady) 'Older single-tunnel health is unsupported.'
    $checks.Add('authenticated owner health, runtime validation and partial tunnel readiness')
    Set-Item Function:\Get-LauncherHttp $originalHttp

    $originalCli = (Get-Command Invoke-LauncherCli).ScriptBlock
    $originalJson = (Get-Command Get-LauncherJson).ScriptBlock
    $originalConnections = (Get-Command Get-LauncherConnections).ScriptBlock
    $script:calls = New-Object 'Collections.Generic.List[string]'
    $script:fakeService = [pscustomobject]@{ installed = $true; running = $true }
    $script:fakeBrowser = [pscustomobject]@{ state = 'ready' }
    function Get-LauncherJson($Context, [string[]]$Arguments) {
        if ($Arguments[0] -eq 'service') { return $script:fakeService }
        return $script:fakeBrowser
    }
    function Invoke-LauncherCli($Context, [string[]]$Arguments, [int]$TimeoutSeconds = 30, [switch]$AllowFailure) {
        $command = $Arguments -join ' '
        $script:calls.Add($command)
        switch ($command) {
            'service start' { $script:fakeService.running = $true }
            'service install' { $script:fakeService.installed = $true; $script:fakeService.running = $true }
            'browser start' { $script:fakeBrowser.state = 'ready' }
            'service stop' { $script:fakeService.running = $false }
            'browser stop' { $script:fakeBrowser.state = 'stopped' }
        }
        [pscustomobject]@{ ExitCode = 0; Output = '' }
    }
    function Get-LauncherConnections($Context, $Settings) {
        [pscustomobject]@{ ServerReady = $script:fakeService.running; ChatReady = $script:fakeService.running; TunnelReady = $script:fakeService.running; ChatPort = 3120; TunnelsReady = 1; TunnelsConfigured = 1 }
    }
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($null -eq $result.Error -and $result.Snapshot.Ready -and $script:calls.Count -eq 0) 'Start disturbed an existing healthy service or browser.'
    $script:fakeBrowser.state = 'busy'
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($null -eq $result.Error -and $script:calls.Count -eq 0) 'Start disturbed a busy browser.'
    $checks.Add('idempotent startup preserves healthy and busy sessions')
    $script:fakeService.running = $false
    $script:fakeBrowser.state = 'stopped'
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($null -eq $result.Error -and $result.Snapshot.Ready -and ($script:calls -join ',') -eq 'service start,browser start') 'Stopped services were not started in order.'
    $script:calls.Clear()
    $result = Invoke-LauncherAction $context 'Stop'
    Assert-Launcher ($null -eq $result.Error -and -not $result.Snapshot.Ready -and ($script:calls -join ',') -eq 'service stop,browser stop') 'Stop did not stop the supervisor before the browser.'
    $checks.Add('start and stop order for the service, tunnel and browser')
    $script:calls.Clear()
    $script:fakeService.installed = $false
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($null -eq $result.Error -and $script:calls[0] -eq 'service install') 'Missing service was not installed with the saved config.'
    $script:calls.Clear()
    $script:fakeBrowser.state = 'authentication_required'
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($result.Error -like '*Sign in*' -and -not $result.Snapshot.Ready -and $script:calls.Count -eq 0) 'Expired login did not produce a recoverable sign-in state.'
    $checks.Add('service installation and expired sign-in recovery')
    [IO.File]::WriteAllText($context.ConfigPath, '{"agentChat":{"enabled":true}}')
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($result.Error -like '*No saved OpenAI tunnel*' -and $script:calls.Count -eq 0) 'A missing tunnel caused unintended startup.'
    [IO.File]::WriteAllText($context.ConfigPath, '{"agentChat":{"enabled":false},"openaiTunnel":{}}')
    $result = Invoke-LauncherAction $context 'Start'
    Assert-Launcher ($result.Error -like '*Agent chat is disabled*' -and $script:calls.Count -eq 0) 'Disabled chat was silently overridden.'
    $checks.Add('missing tunnel and disabled chat fail without configuration changes')
    Set-Item Function:\Invoke-LauncherCli $originalCli
    Set-Item Function:\Get-LauncherJson $originalJson
    Set-Item Function:\Get-LauncherConnections $originalConnections

    # Exercise native shortcut persistence without changing Desktop, Start menu or Startup.
    $shortcutPath = Join-Path $fixture 'fixture launcher.lnk'
    $uiPath = Join-Path $fixture 'a path with spaces\launcher.ps1'
    Set-LauncherShortcut $shortcutPath $uiPath $context.Executable
    $shell = New-Object -ComObject WScript.Shell
    try {
        $shortcut = $shell.CreateShortcut($shortcutPath)
        Assert-Launcher ($shortcut.Arguments -eq (Get-LauncherShortcutArguments $uiPath)) 'Saved shortcut arguments changed.'
        Assert-Launcher ($shortcut.TargetPath -like '*WindowsPowerShell*v1.0*powershell.exe') 'Shortcut does not use the Windows WPF runtime.'
    } finally { $null = [Runtime.InteropServices.Marshal]::ReleaseComObject($shell) }
    $checks.Add('native shortcut persistence with spaces and STA runtime')

    $preview = Join-Path $fixture 'preview.png'
    & (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe') -NoLogo -NoProfile -STA -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'windows-launcher.ps1') -PreviewPath $preview
    Assert-Launcher ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $preview) -and (Get-Item -LiteralPath $preview).Length -gt 10000) 'The WPF preview did not render.'
    $checks.Add('native WPF window creation and PNG rendering')
    [pscustomobject]@{ ok = $true; checks = $checks.ToArray() } | ConvertTo-Json -Depth 3
} finally {
    $resolvedFixture = [IO.Path]::GetFullPath($fixture)
    if ($resolvedFixture.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and (Split-Path -Leaf $resolvedFixture) -match '^codexify-launcher-test-[a-f0-9]{32}$') {
        Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
    }
}
