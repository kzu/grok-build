# Remove the grok binaries installed by scripts/install.ps1.
# Leaves %USERPROFILE%\.grok\config.toml, auth, and sessions in place.
#
#   irm https://github.com/kzu/grok-build/releases/latest/download/uninstall.ps1 | iex

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

if ($PSVersionTable.Platform -and $PSVersionTable.Platform -ne 'Win32NT') {
    Write-Error "This uninstaller is for Windows PowerShell. On Linux and WSL, run uninstall.sh."
    exit 1
}

$BinDir = if ($env:GROK_BIN_DIR) { $env:GROK_BIN_DIR } else { Join-Path $env:USERPROFILE '.grok\bin' }
$DownloadDir = if ($env:GROK_DOWNLOAD_DIR) { $env:GROK_DOWNLOAD_DIR } else { Join-Path $env:USERPROFILE '.grok\downloads' }

foreach ($name in @('grok.exe', 'agent.exe', 'grok.exe.old', 'agent.exe.old')) {
    $path = Join-Path $BinDir $name
    if (Test-Path $path) { Remove-Item -Force $path }
}
if (Test-Path $DownloadDir) {
    Get-ChildItem -Path $DownloadDir -File -Filter 'grok-*' | Remove-Item -Force
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath) {
    $kept = @($userPath -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -ine $BinDir.TrimEnd('\')) })
    [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User')
}
$envParts = @($env:Path -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -ine $BinDir.TrimEnd('\')) })
$env:Path = $envParts -join ';'

Write-Host "grok was uninstalled. Config and sessions under $($env:USERPROFILE)\.grok were kept."
Write-Host "Open a new terminal so PATH picks up the change."
