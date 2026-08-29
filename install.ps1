$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ProgressPreference = 'SilentlyContinue'

$Repository = if ($env:CODEXIFY_GITHUB_REPOSITORY) { $env:CODEXIFY_GITHUB_REPOSITORY } else { 'devnoname120/codexify' }
$InstallDir = Join-Path $HOME '.codexify\bin'
$ReleaseRoot = if ($env:CODEXIFY_RELEASE_ROOT) { $env:CODEXIFY_RELEASE_ROOT.TrimEnd('/') } else { "https://github.com/$Repository/releases/download" }
$Version = $env:CODEXIFY_VERSION
$Headers = @{ 'User-Agent' = 'codexify-installer'; 'Accept' = 'application/vnd.github+json' }

[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

if (-not $Version) {
    $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest" -Headers $Headers
    $Version = [string]$Release.tag_name
}

if ($Version -notmatch '^[A-Za-z0-9._-]+$') {
    throw "Invalid release tag: $Version"
}

$Architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
$Platforms = switch ($Architecture) {
    'x64' { @('windows-x64') }
    'arm64' { @('windows-arm64', 'windows-x64') }
    default { throw "Unsupported Windows architecture: $Architecture" }
}

$TempDir = Join-Path ([IO.Path]::GetTempPath()) ("codexify-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $TempDir | Out-Null

try {
    $ChecksumsPath = Join-Path $TempDir 'checksums.txt'
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseRoot/$Version/checksums.txt" -Headers $Headers -OutFile $ChecksumsPath

    $ArchivePath = $null
    $Asset = $null
    foreach ($Platform in $Platforms) {
        $Candidate = "codexify-$Version-$Platform.zip"
        $CandidatePath = Join-Path $TempDir $Candidate
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseRoot/$Version/$Candidate" -Headers $Headers -OutFile $CandidatePath
            $Asset = $Candidate
            $ArchivePath = $CandidatePath
            break
        }
        catch {
            Remove-Item -Force -ErrorAction SilentlyContinue $CandidatePath
        }
    }

    if (-not $ArchivePath) {
        throw "No compatible Windows release asset was found for $Architecture"
    }

    $Pattern = '^[0-9a-fA-F]{64}\s+\*?' + [Regex]::Escape($Asset) + '$'
    $ChecksumLine = Get-Content -LiteralPath $ChecksumsPath | Where-Object { $_ -match $Pattern } | Select-Object -First 1
    if (-not $ChecksumLine) {
        throw "checksums.txt does not contain $Asset"
    }

    $Expected = (($ChecksumLine -split '\s+')[0]).ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash.ToLowerInvariant()
    if ($Expected -ne $Actual) {
        throw "Checksum mismatch for $Asset"
    }

    $ExtractDir = Join-Path $TempDir 'extract'
    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $ExtractDir
    $Binaries = @(Get-ChildItem -LiteralPath $ExtractDir -Recurse -File -Filter 'codexify.exe')
    if ($Binaries.Count -ne 1) {
        throw "Release archive contains $($Binaries.Count) codexify.exe files; expected exactly one"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Target = Join-Path $InstallDir 'codexify.exe'
    $Staged = Join-Path $InstallDir ('.codexify.new.' + $PID + '.exe')
    Copy-Item -LiteralPath $Binaries[0].FullName -Destination $Staged -Force
    Remove-Item -LiteralPath $Target -Force -ErrorAction SilentlyContinue
    Move-Item -LiteralPath $Staged -Destination $Target

    & $Target --help *> $null
    if ($LASTEXITCODE -ne 0) {
        throw 'The installed executable did not start successfully'
    }

    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $Entries = if ($UserPath) { @($UserPath -split ';' | Where-Object { $_ }) } else { @() }
    $Present = $Entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $InstallDir.TrimEnd('\') }
    if (-not $Present) {
        $UpdatedPath = if ($UserPath) { "$($UserPath.TrimEnd(';'));$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $UpdatedPath, 'User')
        Write-Host "Added $InstallDir to the user PATH"
    }
    if (-not (($env:Path -split ';') | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $InstallDir.TrimEnd('\') })) {
        $env:Path = "$InstallDir;$env:Path"
    }

    Write-Host "Installed Codexify $Version to $Target"
    Write-Host 'Open a new terminal to use codexify from PATH.'
}
finally {
    Remove-Item -LiteralPath $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}
