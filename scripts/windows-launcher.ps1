[CmdletBinding()]
param(
    [switch]$NoAutoStart,
    [string]$PreviewPath,
    [string]$CapturePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName PresentationFramework, PresentationCore, WindowsBase
. (Join-Path $PSScriptRoot 'windows-launcher-core.ps1')

$context = New-LauncherContext
$mutex = $null
$activation = $null
$ownsMutex = $false
$script:job = $null
$script:pendingAction = $null
$script:snapshot = $null
$script:lastError = $null
$script:lastPhase = ''
$script:nextRefresh = [DateTime]::UtcNow
$script:capture = $CapturePath
$script:progressState = [hashtable]::Synchronized(@{ Message = '' })
$script:preferences = @{ OpenChat = $true }
$script:startupPath = Join-Path ([Environment]::GetFolderPath('Startup')) 'Codexify Launcher.lnk'
$scriptPath = $PSCommandPath
$corePath = Join-Path $PSScriptRoot 'windows-launcher-core.ps1'
$timer = $null

function Write-LauncherActivity([string]$Message) {
    $safe = Protect-LauncherText $Message
    $line = '[{0}] {1}' -f [DateTime]::Now.ToString('HH:mm:ss'), $safe
    $ui.ActivityText.AppendText($line + [Environment]::NewLine)
    if ($ui.ActivityText.LineCount -gt 120) { $ui.ActivityText.Text = ($ui.ActivityText.Text -split '\r?\n' | Select-Object -Last 90) -join [Environment]::NewLine }
    $ui.ActivityText.ScrollToEnd()
    if ($script:capture) { Add-Content -LiteralPath ($script:capture + '.log') -Value $line -Encoding UTF8 }
}

function Save-LauncherPreferences {
    $null = New-Item -ItemType Directory -Path $context.LauncherDirectory -Force
    $script:preferences.OpenChat = $ui.OpenChatCheck.IsChecked -eq $true
    $path = Join-Path $context.LauncherDirectory 'preferences.json'
    $temporary = $path + '.tmp'
    $script:preferences | ConvertTo-Json | Set-Content -LiteralPath $temporary -Encoding UTF8
    Move-Item -LiteralPath $temporary -Destination $path -Force
}

function Set-LauncherCard([string]$Name, [string]$Text, [string]$Detail, [bool]$Ready) {
    $ui[$Name + 'State'].Text = $Text
    $ui[$Name + 'State'].Foreground = if ($Ready) { '#92E3C0' } else { '#D8B47A' }
    $ui[$Name + 'Detail'].Text = $Detail
}

function Update-LauncherDisplay {
    $busy = $null -ne $script:job
    foreach ($name in @('StartButton', 'StopButton', 'LoginButton', 'RevealButton', 'RefreshButton')) { $ui[$name].IsEnabled = -not $busy }
    $ui.BusyBar.Visibility = if ($busy) { 'Visible' } else { 'Hidden' }
    $ui.OpenButton.IsEnabled = $null -ne $script:snapshot -and $script:snapshot.Connections.ChatReady
    if ($null -eq $script:snapshot) {
        if ($script:lastError) {
            $ui.Headline.Text = 'Your setup needs attention.'
            $ui.ServiceBadge.Text = 'Setup unavailable'
            foreach ($name in @('Chat', 'Tunnel', 'Browser')) { Set-LauncherCard $name 'Unavailable' 'Check the message below' $false }
        }
        return
    }
    $state = $script:snapshot
    $con = $state.Connections
    $ui.ServiceBadge.Text = if ($state.Service.running) { 'Service running' } else { 'Service stopped' }
    $ui.Headline.Text = if ($state.Ready) { 'Everything is connected.' } elseif ($busy) { 'Getting your workspace ready...' } else { 'Your workspace is waiting.' }
    $chatLabel = if ($con.ChatReady) { 'Online' } else { 'Offline' }
    $chatDetail = if ($con.ChatPort) { 'Localhost port ' + $con.ChatPort } else { 'Waiting for the chat listener' }
    Set-LauncherCard 'Chat' $chatLabel $chatDetail $con.ChatReady
    $tunnelLabel = if ($con.TunnelReady) { 'Connected' } elseif ($state.Settings.TunnelCount -eq 0) { 'No tunnel' } else { 'Offline' }
    $tunnelDetail = if ($null -ne $con.TunnelsReady) { '{0} of {1} tunnels ready' -f $con.TunnelsReady, $con.TunnelsConfigured } else { '{0} saved tunnel(s)' -f $state.Settings.TunnelCount }
    Set-LauncherCard 'Tunnel' $tunnelLabel $tunnelDetail $con.TunnelReady
    $browserLabel = switch ($state.BrowserState) {
        'ready' { 'Ready' }; 'busy' { 'Working' }; 'authentication_required' { 'Sign in' }
        'account_changed' { 'Sign in' }; 'stopped' { 'Stopped' }; 'not_installed' { 'Missing' }
        'disabled' { 'Disabled' }; default { 'Check browser' }
    }
    $browserDetail = if ($state.BrowserState -in @('authentication_required', 'account_changed')) { 'Use Sign in to restore your session' } elseif ($state.BrowserReady -and $state.Settings.BrowserEnabled) { 'Saved ChatGPT session restored' } else { 'Use Browser or Sign in below' }
    Set-LauncherCard 'Browser' $browserLabel $browserDetail ($state.BrowserReady -and $state.Settings.BrowserEnabled)
    $ui.LoginButton.IsEnabled = -not $busy -and $state.Settings.BrowserEnabled
    $ui.RevealButton.IsEnabled = -not $busy -and $state.Settings.BrowserEnabled
    $ui.ConfigText.Text = $context.ConfigPath
    $ui.ConfigText.ToolTip = $context.ConfigPath
    $project = $state.Settings.WorkDirectory -replace '^\\\\\?\\', ''
    $ui.ProjectText.Text = if ($project) { 'Projects: ' + $project } else { 'Using the saved project settings' }
    $ui.ProjectText.ToolTip = $ui.ProjectText.Text
    $ui.LastChecked.Text = 'Checked ' + $state.CheckedAt
}

function Save-LauncherCapture([string]$Path) {
    $window.UpdateLayout()
    $bitmap = New-Object Windows.Media.Imaging.RenderTargetBitmap([int]$window.ActualWidth, [int]$window.ActualHeight, 96, 96, [Windows.Media.PixelFormats]::Pbgra32)
    $bitmap.Render($window)
    $encoder = New-Object Windows.Media.Imaging.PngBitmapEncoder
    $encoder.Frames.Add([Windows.Media.Imaging.BitmapFrame]::Create($bitmap))
    $stream = [IO.File]::Create([IO.Path]::GetFullPath($Path))
    try { $encoder.Save($stream) } finally { $stream.Dispose() }
}

function Start-LauncherAction([string]$Action) {
    if ($null -ne $script:job) { $script:pendingAction = $Action; return }
    $script:lastError = $null
    $script:lastPhase = ''
    $script:progressState.Message = 'Checking saved setup...'
    $pipeline = [PowerShell]::Create()
    $null = $pipeline.AddScript('param($core, $ctx, $action, $progress); $ErrorActionPreference = "Stop"; . $core; Invoke-LauncherAction $ctx $action $progress').AddArgument($corePath).AddArgument($context).AddArgument($Action).AddArgument($script:progressState)
    try {
        $script:job = @{ Pipeline = $pipeline; Handle = $pipeline.BeginInvoke(); Action = $Action }
    } catch { $pipeline.Dispose(); throw }
    if ($Action -ne 'Refresh') { Write-LauncherActivity ($Action + ' requested.') }
    Update-LauncherDisplay
}

try {
    if (-not $PreviewPath) {
        $created = $false
        $mutex = New-Object Threading.Mutex($false, 'Local\Codexify.WindowsLauncher', [ref]$created)
        $activation = New-Object Threading.EventWaitHandle($false, [Threading.EventResetMode]::ManualReset, 'Local\Codexify.WindowsLauncher.Activate')
        try { $ownsMutex = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $ownsMutex = $true }
        if (-not $ownsMutex) { $null = $activation.Set(); return }
        $prefsPath = Join-Path $context.LauncherDirectory 'preferences.json'
        if (Test-Path -LiteralPath $prefsPath) {
            try { $saved = Get-Content -LiteralPath $prefsPath -Raw | ConvertFrom-Json; $script:preferences.OpenChat = (Get-LauncherProperty $saved 'OpenChat' $true) -eq $true } catch {}
        }
    }
    [xml]$xaml = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'windows-launcher.xaml') -Raw
    $reader = New-Object Xml.XmlNodeReader $xaml
    $window = [Windows.Markup.XamlReader]::Load($reader)
    $ui = @{}
    $names = New-Object Xml.XmlNamespaceManager($xaml.NameTable)
    $names.AddNamespace('x', 'http://schemas.microsoft.com/winfx/2006/xaml')
    foreach ($node in $xaml.SelectNodes('//*[@x:Name]', $names)) { $name = $node.GetAttribute('Name', 'http://schemas.microsoft.com/winfx/2006/xaml'); $ui[$name] = $window.FindName($name) }
    $ui.ConfigText.Text = $context.ConfigPath
    $ui.OpenChatCheck.IsChecked = $script:preferences.OpenChat
    $ui.StartupCheck.IsChecked = -not $PreviewPath -and (Test-Path -LiteralPath $script:startupPath)
    $ui.StartButton.Add_Click({ Start-LauncherAction 'Start' })
    $ui.StopButton.Add_Click({ Start-LauncherAction 'Stop' })
    $ui.RefreshButton.Add_Click({ Start-LauncherAction 'Refresh' })
    $ui.LoginButton.Add_Click({ Start-LauncherAction 'Login' })
    $ui.RevealButton.Add_Click({ Start-LauncherAction 'Reveal' })
    $ui.OpenButton.Add_Click({ try { Open-LauncherChat $context; Write-LauncherActivity 'Opened the private chat in your default browser.' } catch { $ui.StatusText.Text = Protect-LauncherText $_.Exception.Message } })
    $ui.OpenChatCheck.Add_Click({ try { Save-LauncherPreferences } catch { $ui.StatusText.Text = 'Could not save the launcher preference.' } })
    $ui.StartupCheck.Add_Click({
        try {
            if ($ui.StartupCheck.IsChecked) {
                Set-LauncherShortcut $script:startupPath $scriptPath $context.Executable
                Write-LauncherActivity 'Launcher will open and start services at Windows sign-in.'
            } else {
                if (Test-Path -LiteralPath $script:startupPath) { Remove-Item -LiteralPath $script:startupPath }
                Write-LauncherActivity 'Launcher sign-in shortcut removed.'
            }
        } catch { $ui.StartupCheck.IsChecked = Test-Path -LiteralPath $script:startupPath; $ui.StatusText.Text = Protect-LauncherText $_.Exception.Message }
    })
    if ($PreviewPath) {
        $script:snapshot = [pscustomobject]@{
            Service = [pscustomobject]@{ running = $true }; BrowserState = 'ready'; BrowserReady = $true; Ready = $true; CheckedAt = '12:00:00'
            Settings = [pscustomobject]@{ TunnelCount = 1; BrowserEnabled = $true; WorkDirectory = 'C:\Projects'; ConnectorName = 'Codexify' }
            Connections = [pscustomobject]@{ ChatReady = $true; ChatPort = 3120; TunnelReady = $true; TunnelsReady = 1; TunnelsConfigured = 1 }
        }
        Update-LauncherDisplay
        $ui.StatusText.Text = 'Design preview. No services have been started.'
        Write-LauncherActivity 'Preview of the connected launcher.'
        $window.Add_ContentRendered({ Save-LauncherCapture $PreviewPath; $window.Close() })
    } else {
        $timer = New-Object Windows.Threading.DispatcherTimer
        $timer.Interval = [TimeSpan]::FromMilliseconds(200)
        $timer.Add_Tick({
            try {
                if ($activation.WaitOne(0)) {
                    $null = $activation.Reset()
                    if ($window.WindowState -eq 'Minimized') { $window.WindowState = 'Normal' }
                    $null = $window.Activate()
                }
                if ($script:job) {
                    $phase = [string]$script:progressState.Message
                    if ($phase -ne $script:lastPhase) {
                        $script:lastPhase = $phase
                        $ui.StatusText.Text = $phase
                        if ($script:job.Action -ne 'Refresh') { Write-LauncherActivity $phase }
                    }
                    if ($script:job.Handle.IsCompleted) {
                        $completed = $script:job
                        $script:job = $null
                        try {
                            $results = $completed.Pipeline.EndInvoke($completed.Handle)
                            if ($results.Count -ne 1) { throw 'The launcher did not receive a status result.' }
                            $result = $results[0]
                            $script:snapshot = $result.Snapshot
                            $script:lastError = $result.Error
                        } catch { $script:lastError = Protect-LauncherText $_.Exception.Message }
                        finally { $completed.Pipeline.Dispose() }
                        $script:nextRefresh = [DateTime]::UtcNow.AddSeconds(10)
                        if ($script:lastError) {
                            $ui.StatusText.Text = $script:lastError
                            $ui.StatusText.Foreground = '#E8B37F'
                            Write-LauncherActivity $script:lastError
                        } else {
                            $ui.StatusText.Foreground = '#BDC9D5'
                            $ui.StatusText.Text = if ($script:snapshot.Ready) { 'Chat, tunnel and browser are ready.' } else { 'Use Start all to connect, or check the status above.' }
                            if ($completed.Action -ne 'Refresh') { Write-LauncherActivity ($completed.Action + ' finished. ' + $ui.StatusText.Text) }
                            if ($completed.Action -eq 'Start' -and $script:preferences.OpenChat -and $script:snapshot.Connections.ChatReady) {
                                try { Open-LauncherChat $context; Write-LauncherActivity 'Opened the private chat in your default browser.' } catch { Write-LauncherActivity (Protect-LauncherText $_.Exception.Message) }
                            }
                        }
                        Update-LauncherDisplay
                        if ($script:capture) { try { Save-LauncherCapture $script:capture; $script:capture = '' } catch { Write-LauncherActivity ('Could not save the UI capture: ' + $_.Exception.Message) } }
                    }
                } elseif ($script:pendingAction) {
                    $action = $script:pendingAction; $script:pendingAction = $null; Start-LauncherAction $action
                } elseif ([DateTime]::UtcNow -ge $script:nextRefresh) { Start-LauncherAction 'Refresh' }
            } catch { $ui.StatusText.Text = Protect-LauncherText $_.Exception.Message; if ($script:capture) { Add-Content -LiteralPath ($script:capture + '.log') -Value $ui.StatusText.Text -Encoding UTF8 } }
        })
        $window.Add_ContentRendered({
            Write-LauncherActivity 'Using the installed Codexify configuration and saved browser profile.'
            if ($NoAutoStart) { Start-LauncherAction 'Refresh' } else { Start-LauncherAction 'Start' }
            $timer.Start()
        })
    }
    $window.Add_Closing({
        if ($script:job -and $script:job.Action -ne 'Refresh') {
            $_.Cancel = $true
            $ui.StatusText.Text = 'Please let the current action finish before closing.'
        }
    })
    $null = $window.ShowDialog()
} catch {
    $safe = Protect-LauncherText $_.Exception.Message
    if (-not $PreviewPath -and -not $CapturePath) { $null = [Windows.MessageBox]::Show($safe, 'Codexify Launcher', 'OK', 'Error') }
    throw
} finally {
    if ($timer) { $timer.Stop() }
    if ($script:job) { $script:job.Pipeline.Stop(); $script:job.Pipeline.Dispose() }
    if ($activation) { $activation.Dispose() }
    if ($ownsMutex) { $mutex.ReleaseMutex() }
    if ($mutex) { $mutex.Dispose() }
}
