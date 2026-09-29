<#
.SYNOPSIS
    Autumn CLI installer for Windows (x86_64-pc-windows-msvc).

.DESCRIPTION
    Downloads a prebuilt `autumn.exe` from GitHub Releases, verifies its sha256
    checksum, and installs it. This is the Windows counterpart to the POSIX
    scripts/install.sh (which covers Linux and macOS). Binaries correspond to
    tagged crate releases; "latest" is the most recent release.

    Run it directly:
        irm https://raw.githubusercontent.com/autumn-foundation/autumn/trunk-dev/scripts/install.ps1 | iex

    Or with options after downloading:
        .\install.ps1 -Version v0.6.0 -Dir C:\tools\autumn

.PARAMETER Version
    Version tag to install, or "latest" (default: latest). Env override: AUTUMN_VERSION.

.PARAMETER Dir
    Install directory (default: %LOCALAPPDATA%\autumn\bin). Env override: AUTUMN_INSTALL_DIR.

.PARAMETER BaseUrl
    Release base URL (default: https://github.com/autumn-foundation/autumn/releases).
    Env override: AUTUMN_BASE_URL.
#>
[CmdletBinding()]
param(
    [string]$Version = $(if ($env:AUTUMN_VERSION) { $env:AUTUMN_VERSION } else { 'latest' }),
    [string]$Dir     = $(if ($env:AUTUMN_INSTALL_DIR) { $env:AUTUMN_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'autumn\bin' }),
    [string]$BaseUrl = $(if ($env:AUTUMN_BASE_URL) { $env:AUTUMN_BASE_URL } else { 'https://github.com/autumn-foundation/autumn/releases' })
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Windows PowerShell 5.1 doesn't enable TLS 1.2 by default, which breaks GitHub
# downloads. Opt in explicitly before any network call.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$target = 'x86_64-pc-windows-msvc'
$asset  = "autumn-$target.zip"

if ($Version -eq 'latest') {
    $url = "$BaseUrl/latest/download/$asset"
} else {
    $url = "$BaseUrl/download/$Version/$asset"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("autumn-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $zipPath = Join-Path $tmp $asset
    $shaPath = "$zipPath.sha256"

    Write-Host "autumn-install: downloading $asset ($Version)"
    Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing
    Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaPath -UseBasicParsing

    # The .sha256 file is "<hash>  <filename>" (sha256sum format); take the first field.
    $expected = ((Get-Content -Path $shaPath -Raw).Trim() -split '\s+')[0].ToLower()
    if (-not $expected) { throw "empty checksum from $url.sha256" }
    $actual = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "checksum mismatch: expected $expected, got $actual"
    }
    Write-Host "autumn-install: checksum ok ($actual)"

    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
    $exe = Join-Path $tmp 'autumn.exe'
    if (-not (Test-Path $exe)) { throw "archive did not contain autumn.exe" }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $dest = Join-Path $Dir 'autumn.exe'
    Copy-Item -Path $exe -Destination $dest -Force
    Write-Host "autumn-install: installed autumn -> $dest"

    $pathEntries = ($env:PATH -split ';') | ForEach-Object { $_.TrimEnd('\') }
    $normalizedDir = $Dir.TrimEnd('\')
    if ($pathEntries -notcontains $normalizedDir) {
        Write-Host "autumn-install: note: $Dir is not on your PATH."
        Write-Host "  Add it for the current session:  `$env:PATH = `"$Dir;`$env:PATH`""
        Write-Host "  Or persist it for your user:"
        Write-Host "    [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$Dir', 'User')"
    }

    & $dest --version
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
