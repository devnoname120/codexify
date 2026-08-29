$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ProgressPreference = 'SilentlyContinue'

if (-not $IsWindows) {
    throw 'This test requires Windows.'
}

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$Root = Join-Path ([IO.Path]::GetTempPath()) ("codexify-installer-test-" + [Guid]::NewGuid().ToString('N'))
$ServeRoot = Join-Path $Root 'releases'
$InstallDir = Join-Path $Root 'install-bin'
$Marker = Join-Path $Root 'service-calls.txt'
$Version = 'v9.9.9'
$Platform = 'windows-x64'
$Asset = "codexify-$Version-$Platform.zip"
$ReleaseDir = Join-Path $ServeRoot $Version
$Stage = Join-Path $Root "codexify-$Version-$Platform"
$Server = $null
$OriginalUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$OriginalProcessPath = $env:Path
$EnvironmentNames = @(
    'CODEXIFY_VERSION',
    'CODEXIFY_RELEASE_ROOT',
    'CODEXIFY_INSTALL_DIR',
    'CODEXIFY_SKIP_SERVICE',
    'CODEXIFY_TEST_SERVICE_MARKER'
)
$OriginalEnvironment = @{}
foreach ($Name in $EnvironmentNames) {
    $OriginalEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process')
}

function Restore-Environment {
    foreach ($Name in $EnvironmentNames) {
        $Value = $OriginalEnvironment[$Name]
        if ($null -eq $Value) {
            Remove-Item -Path "Env:$Name" -ErrorAction SilentlyContinue
        }
        else {
            Set-Item -Path "Env:$Name" -Value $Value
        }
    }
    $env:Path = $OriginalProcessPath
    [Environment]::SetEnvironmentVariable('Path', $OriginalUserPath, 'User')
}

try {
    New-Item -ItemType Directory -Path $ReleaseDir, $Stage, $InstallDir | Out-Null
    $Source = Join-Path $Root 'fake-codexify.rs'
    @'
use std::env;
use std::fs::OpenOptions;
use std::io::Write;

fn record(value: &str) {
    let path = env::var_os("CODEXIFY_TEST_SERVICE_MARKER").expect("marker path");
    let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{value}").unwrap();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [arg] if arg == "--help" => {}
        [service, arg] if service == "service" && arg == "--help" => {}
        [service, action] if service == "service" && action == "install" => record("install"),
        [service, action] if service == "service" && action == "disable" => record("disable"),
        _ => std::process::exit(2),
    }
}
'@ | Set-Content -LiteralPath $Source -Encoding utf8NoBOM

    $FakeBinary = Join-Path $Stage 'codexify.exe'
    & rustc $Source -o $FakeBinary
    if ($LASTEXITCODE -ne 0) {
        throw 'Failed to compile the fake Codexify executable.'
    }

    $Archive = Join-Path $ReleaseDir $Asset
    Compress-Archive -LiteralPath $Stage -DestinationPath $Archive
    $Hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash.ToLowerInvariant()
    "$Hash  $Asset" | Set-Content -LiteralPath (Join-Path $ReleaseDir 'checksums.txt') -Encoding ascii

    $Listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $Listener.Start()
    $Port = ([Net.IPEndPoint]$Listener.LocalEndpoint).Port
    $Listener.Stop()
    $Python = (Get-Command python.exe -ErrorAction Stop).Source
    $Server = Start-Process -FilePath $Python -WorkingDirectory $ServeRoot -ArgumentList @(
        '-m', 'http.server', "$Port", '--bind', '127.0.0.1'
    ) -PassThru -WindowStyle Hidden

    $Ready = $false
    for ($Attempt = 0; $Attempt -lt 40; $Attempt++) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/$Version/checksums.txt" | Out-Null
            $Ready = $true
            break
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $Ready) {
        throw 'Local release server did not start.'
    }

    $env:CODEXIFY_VERSION = $Version
    $env:CODEXIFY_RELEASE_ROOT = "http://127.0.0.1:$Port"
    $env:CODEXIFY_INSTALL_DIR = $InstallDir
    $env:CODEXIFY_TEST_SERVICE_MARKER = $Marker
    Remove-Item Env:CODEXIFY_SKIP_SERVICE -ErrorAction SilentlyContinue

    & (Join-Path $RepositoryRoot 'install.ps1')
    & (Join-Path $RepositoryRoot 'install.ps1')
    $env:CODEXIFY_SKIP_SERVICE = '1'
    & (Join-Path $RepositoryRoot 'install.ps1')
    Remove-Item Env:CODEXIFY_SKIP_SERVICE -ErrorAction SilentlyContinue

    $Target = Join-Path $InstallDir 'codexify.exe'
    if (-not (Test-Path -LiteralPath $Target -PathType Leaf)) {
        throw 'Installer did not publish codexify.exe.'
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $Target).Hash -ne (Get-FileHash -Algorithm SHA256 -LiteralPath $FakeBinary).Hash) {
        throw 'Installed executable does not match the release asset.'
    }

    $Calls = @(Get-Content -LiteralPath $Marker)
    if (@($Calls | Where-Object { $_ -eq 'install' }).Count -ne 2) {
        throw 'Installer did not install the service after each executable replacement.'
    }
    if (@($Calls | Where-Object { $_ -eq 'disable' }).Count -ne 1) {
        throw 'Installer did not disable the existing service before replacement.'
    }

    $NormalizedInstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
    $PathMatches = @(
        ([Environment]::GetEnvironmentVariable('Path', 'User') -split ';') |
            Where-Object {
                $_ -and [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $NormalizedInstallDir
            }
    )
    if ($PathMatches.Count -ne 1) {
        throw 'Installer did not add the installation directory to the user PATH exactly once.'
    }

    Write-Host 'Windows installer replacement, checksum, PATH, and service integration test: PASS'
}
finally {
    if ($Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
        $Server.WaitForExit()
    }
    Restore-Environment
    Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
}
