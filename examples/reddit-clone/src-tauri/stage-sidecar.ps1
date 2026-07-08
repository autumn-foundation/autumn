# Build the autumn server sidecar with embedded assets and managed Postgres,
# then place it in src-tauri\binaries\ for Tauri to bundle.
#
# Run manually: powershell -File src-tauri\stage-sidecar.ps1
# Or set tauri.conf.json > build.beforeBuildCommand to:
#   "powershell -ExecutionPolicy Bypass -File stage-sidecar.ps1"
$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppDir = Split-Path -Parent $ScriptDir
Set-Location $AppDir
# TAURI_ENV_TARGET_TRIPLE is set by `cargo tauri build` for cross-compilation;
# fall back to the host triple when running the script manually.
$TargetTriple = $Env:TAURI_ENV_TARGET_TRIPLE
if (-not $TargetTriple) {
    $TargetTriple = (rustc -Vv | Select-String "^host").Line.Split()[1]
}
# Resolve the real Cargo output directory.  Workspace members share the workspace
# root's target\ and CARGO_TARGET_DIR / .cargo/config.toml can redirect it.
$TargetDir = $Env:CARGO_TARGET_DIR
if (-not $TargetDir) {
    $TargetDir = (cargo metadata --no-deps --format-version 1 --quiet | ConvertFrom-Json).target_directory
}
# Fingerprint static/ before the embed compile (mirrors autumn build --embed phases 1-2):
# compile → write .autumn-manifest.json → the cargo build below embeds it.
# --features passes managed-pg-bundled so apps wiring ManagedPostgresPoolProvider
# without a cfg gate can compile during the fingerprint phase.
autumn build --embed -p reddit-clone --bin reddit-clone --features autumn-web/managed-pg-bundled
New-Item -ItemType Directory -Force -Path src-tauri\binaries | Out-Null
cargo build --release -p reddit-clone --target "$TargetTriple" --bin reddit-clone `
  --features embed-assets,autumn-web/managed-pg-bundled
Copy-Item "$TargetDir\$TargetTriple\release\reddit-clone.exe" `
          "src-tauri\binaries\reddit-clone-$TargetTriple.exe"
Write-Host "Staged: src-tauri/binaries/reddit-clone-$TargetTriple.exe"
# Stage profile config files into src-tauri\configs\ so tauri.conf.json resource
# entries are always satisfiable at bundle time.
# For alias pairs (prod/production, dev/development): AutumnConfig stops at the
# first existing file in its ordered lookup list.  Copy the available file to
# BOTH names so the profile resolves correctly regardless of AUTUMN_ENV spelling,
# avoiding an empty stub from shadowing real config in the other alias.
New-Item -ItemType Directory -Force -Path src-tauri\configs | Out-Null
# Ensure autumn.toml exists at the project root — tauri.conf.json always
# lists it as a bundle resource.  Projects without a config file use
# AutumnConfig defaults; an empty TOML is a valid no-op.
if (-not (Test-Path autumn.toml)) {
    New-Item -ItemType File -Force -Path autumn.toml | Out-Null
}
# prod/production alias pair
if ((Test-Path autumn-prod.toml) -and (Test-Path autumn-production.toml)) {
    Copy-Item autumn-prod.toml src-tauri\configs\autumn-prod.toml
    Copy-Item autumn-production.toml src-tauri\configs\autumn-production.toml
} elseif (Test-Path autumn-prod.toml) {
    Copy-Item autumn-prod.toml src-tauri\configs\autumn-prod.toml
    Copy-Item autumn-prod.toml src-tauri\configs\autumn-production.toml
} elseif (Test-Path autumn-production.toml) {
    Copy-Item autumn-production.toml src-tauri\configs\autumn-prod.toml
    Copy-Item autumn-production.toml src-tauri\configs\autumn-production.toml
} else {
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-prod.toml | Out-Null
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-production.toml | Out-Null
}
# dev/development alias pair (same logic)
if ((Test-Path autumn-dev.toml) -and (Test-Path autumn-development.toml)) {
    Copy-Item autumn-dev.toml src-tauri\configs\autumn-dev.toml
    Copy-Item autumn-development.toml src-tauri\configs\autumn-development.toml
} elseif (Test-Path autumn-dev.toml) {
    Copy-Item autumn-dev.toml src-tauri\configs\autumn-dev.toml
    Copy-Item autumn-dev.toml src-tauri\configs\autumn-development.toml
} elseif (Test-Path autumn-development.toml) {
    Copy-Item autumn-development.toml src-tauri\configs\autumn-dev.toml
    Copy-Item autumn-development.toml src-tauri\configs\autumn-development.toml
} else {
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-dev.toml | Out-Null
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-development.toml | Out-Null
}
# Standalone profiles (no aliases)
foreach ($f in @("autumn-staging.toml", "autumn-test.toml")) {
    if (Test-Path $f) {
        Copy-Item $f "src-tauri\configs\$f"
    } else {
        New-Item -ItemType File -Force -Path "src-tauri\configs\$f" | Out-Null
    }
}
# Stage encrypted credentials so apps using `config.credentials()` find them at
# AUTUMN_MANIFEST_DIR\config\credentials\<profile>.toml.enc in the installed bundle.
# The staging directory is always created so the tauri.conf.json resource entry
# is satisfiable at bundle time (an empty dir is a no-op for apps with no credentials).
# Note: decryption at runtime requires the AUTUMN_MASTER_KEY env var (or the
# config/master.key file placed in the resource dir).  See the Tauri section
# of the Autumn docs for recommended key distribution strategies.
# Remove and recreate the staging directory so stale .toml.enc files from a
# previous build (deleted or rotated credentials) are not carried into the
# installer.  Autumn loads any .toml.enc it finds via AUTUMN_MANIFEST_DIR, so
# a stale file from a prior build would silently keep a revoked secret active.
if (Test-Path "src-tauri\configs\credentials") {
    Remove-Item -Recurse -Force "src-tauri\configs\credentials"
}
New-Item -ItemType Directory -Force -Path "src-tauri\configs\credentials" | Out-Null
if (Test-Path "config\credentials") {
    # Guard against an empty directory: Copy-Item with a wildcard and
    # $ErrorActionPreference = "Stop" throws when there are no matches.
    $credItems = Get-ChildItem "config\credentials" -ErrorAction SilentlyContinue
    if ($credItems) {
        Copy-Item -Recurse -Force "config\credentials\*" "src-tauri\configs\credentials\"
    }
}
