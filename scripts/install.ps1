# Install grok from this repository's GitHub Releases.
#
#   irm https://github.com/kzu/grok-build/releases/latest/download/install.ps1 | iex
#   $env:GROK_VERSION="1.0.45"; irm https://github.com/kzu/grok-build/releases/latest/download/install.ps1 | iex
#
# Windows PowerShell and PowerShell 7. WSL should use install.sh so it gets
# the Linux binary.
#
# GROK_GITHUB_REPO   default kzu/grok-build
# GROK_BIN_DIR       default %USERPROFILE%\.grok\bin
# GROK_DOWNLOAD_DIR  default %USERPROFILE%\.grok\downloads
# GROK_PRINT_PLAN=1  print the asset URL and exit

param(
    [Parameter(Position = 0)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$ProgressPreference = 'SilentlyContinue'

if (-not $Version -and $env:GROK_VERSION) {
    $Version = $env:GROK_VERSION
}
if ($Version) {
    $Version = $Version -replace '^v', ''
    if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9._]+)?$') {
        Write-Error "Invalid version: $Version (expected X.Y.Z or X.Y.Z-suffix)"
        exit 1
    }
}

if ($PSVersionTable.Platform -and $PSVersionTable.Platform -ne 'Win32NT') {
    Write-Error "This installer is for Windows PowerShell. On Linux and WSL, run: curl -fsSL https://github.com/kzu/grok-build/releases/latest/download/install.sh | bash"
    exit 1
}

$Repo = if ($env:GROK_GITHUB_REPO) { $env:GROK_GITHUB_REPO } else { 'kzu/grok-build' }
$BinDir = if ($env:GROK_BIN_DIR) { $env:GROK_BIN_DIR } else { Join-Path $env:USERPROFILE '.grok\bin' }
$DownloadDir = if ($env:GROK_DOWNLOAD_DIR) { $env:GROK_DOWNLOAD_DIR } else { Join-Path $env:USERPROFILE '.grok\downloads' }

$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default {
        Write-Error "Unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)"
        exit 1
    }
}
$Platform = "windows-$Arch"
if ($Version) {
    $Asset = "grok-$Version-$Platform"
} else {
    $Asset = "grok-$Platform"
}
if ($env:GROK_RELEASE_BASE) {
    $BaseUrl = $env:GROK_RELEASE_BASE.TrimEnd('/')
} elseif ($Version) {
    $BaseUrl = "https://github.com/$Repo/releases/download/v$Version"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
}
$AssetUrl = "$BaseUrl/$Asset"
$SumsUrl = "$BaseUrl/SHA256SUMS"

if ($env:GROK_PRINT_PLAN -eq '1') {
    Write-Output "repo=$Repo"
    Write-Output "platform=$Platform"
    Write-Output "asset=$Asset"
    Write-Output "url=$AssetUrl"
    Write-Output "sums=$SumsUrl"
    Write-Output "bin_dir=$BinDir"
    exit 0
}

New-Item -ItemType Directory -Force -Path $BinDir, $DownloadDir | Out-Null

$Work = Join-Path ([System.IO.Path]::GetTempPath()) ("grok-install-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Force -Path $Work | Out-Null
try {
    $Payload = Join-Path $Work 'grok'
    $Sums = Join-Path $Work 'SHA256SUMS'
    Write-Host "Downloading $Asset..."
    try {
        Invoke-WebRequest -Uri $AssetUrl -OutFile $Payload -UseBasicParsing
    } catch {
        Write-Error "Download failed: $AssetUrl`nPublished platforms are linux-x86_64 and windows-x86_64."
        exit 1
    }
    try {
        Invoke-WebRequest -Uri $SumsUrl -OutFile $Sums -UseBasicParsing
    } catch {
        Write-Error "Download failed: $SumsUrl"
        exit 1
    }

    $Expected = $null
    foreach ($line in Get-Content -Path $Sums) {
        $parts = $line -split '\s+', 2
        if ($parts.Length -eq 2 -and $parts[1] -eq $Asset) {
            $Expected = $parts[0].ToLowerInvariant()
            break
        }
    }
    if (-not $Expected) {
        Write-Error "SHA256SUMS has no entry for $Asset"
        exit 1
    }
    $Actual = (Get-FileHash -Path $Payload -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Write-Error "Checksum mismatch for $Asset`n  expected $Expected`n  actual   $Actual"
        exit 1
    }

    $Stored = Join-Path $DownloadDir $Asset
    Move-Item -Force -Path $Payload -Destination $Stored

    function Install-Exe([string]$SourcePath, [string]$Dest) {
        $old = "$Dest.old"
        if (Test-Path $old) { Remove-Item $old -Force -ErrorAction SilentlyContinue }
        try {
            Copy-Item -Path $SourcePath -Destination $Dest -Force
        } catch {
            if (Test-Path $Dest) { Rename-Item $Dest $old -Force -ErrorAction SilentlyContinue }
            try {
                Copy-Item -Path $SourcePath -Destination $Dest -Force
            } catch {
                if (Test-Path $old) { Rename-Item $old $Dest -Force -ErrorAction SilentlyContinue }
                throw
            }
        }
    }

    Install-Exe $Stored (Join-Path $BinDir 'grok.exe')
    Install-Exe $Stored (Join-Path $BinDir 'agent.exe')
} finally {
    if (Test-Path $Work) { Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue }
}

$Config = Join-Path $env:USERPROFILE '.grok\config.toml'
$GrokHome = Split-Path $Config -Parent
if (-not (Test-Path $GrokHome)) { New-Item -ItemType Directory -Force -Path $GrokHome | Out-Null }
# Marks this install so `grok update` follows this repo's GitHub releases.
$Line = 'installer = "github"'
if (-not (Test-Path $Config)) {
    Set-Content -Path $Config -Value "[cli]`r`n$Line`r`n" -Encoding ascii
} else {
    $text = Get-Content -Raw -Path $Config
    if ($text -match '(?m)^\[cli\]') {
        $text = [regex]::Replace($text, '(?m)^[ \t]*installer[ \t]*=.*\r?\n', '')
        $text = [regex]::Replace($text, '(?m)^\[cli\][ \t]*\r?\n', "[cli]`r`n$Line`r`n")
        Set-Content -Path $Config -Value $text -Encoding ascii -NoNewline
    } else {
        Add-Content -Path $Config -Value "`r`n[cli]`r`n$Line`r`n" -Encoding ascii
    }
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
$parts = @($userPath -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -ine $BinDir.TrimEnd('\')) })
$newPath = (@($BinDir) + $parts) -join ';'
[Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
if ($env:Path -notlike "*$BinDir*") {
    $env:Path = "$BinDir;$env:Path"
}

Write-Host "Installed $BinDir\grok.exe and $BinDir\agent.exe"
Write-Host "Open a new terminal, then run 'grok'."
