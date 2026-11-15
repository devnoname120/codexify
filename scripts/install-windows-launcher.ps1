[CmdletBinding()]
param([switch]$Launch, [switch]$StartWithWindows)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows-launcher-core.ps1')
$context = New-LauncherContext
$null = Get-LauncherSettings $context
$null = New-Item -ItemType Directory -Path $context.LauncherDirectory -Force
foreach ($name in @('windows-launcher.ps1', 'windows-launcher-core.ps1', 'windows-launcher.xaml')) {
    $source = Join-Path $PSScriptRoot $name
    $destination = Join-Path $context.LauncherDirectory $name
    if ([IO.Path]::GetFullPath($source) -ine [IO.Path]::GetFullPath($destination)) {
        Copy-Item -LiteralPath $source -Destination $destination -Force
    }
}
$installedScript = Join-Path $context.LauncherDirectory 'windows-launcher.ps1'
$desktopLink = Join-Path ([Environment]::GetFolderPath('DesktopDirectory')) 'Codexify Launcher.lnk'
$menuLink = Join-Path ([Environment]::GetFolderPath('Programs')) 'Codexify Launcher.lnk'
Set-LauncherShortcut $desktopLink $installedScript $context.Executable
Set-LauncherShortcut $menuLink $installedScript $context.Executable
if ($StartWithWindows) {
    $startupLink = Join-Path ([Environment]::GetFolderPath('Startup')) 'Codexify Launcher.lnk'
    Set-LauncherShortcut $startupLink $installedScript $context.Executable
}
[pscustomobject]@{ Launcher = $installedScript; DesktopShortcut = $desktopLink; StartMenuShortcut = $menuLink }
if ($Launch) {
    $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    Start-Process -FilePath $powershell -ArgumentList (Get-LauncherShortcutArguments $installedScript) -WindowStyle Hidden
}
