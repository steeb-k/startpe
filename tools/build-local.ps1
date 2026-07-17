# SPDX-License-Identifier: GPL-3.0-or-later
# Local release build — CI parity without waiting on CI.
#
# Builds everything a GitHub release would contain and stages it under dist\
# with the exact release asset names:
#   startpe.exe, startpe_loader.dll, syslaunch.exe        (MSVC workspace)
#   SystemInfo.exe, RunBox.exe, Settings.exe,
#   StartMenu.exe, Network.exe                            (MSYS2 ucrt64 helpers)
#
# The GTK helpers MUST be built with MSYS2 ucrt64 (C:\msys64), not MSVC:
# an MSVC build links the gvsbuild DLL names (gtk-4-1.dll) while the PE ships
# winrx-creator's MSYS2 runtime (libgtk-4-1.dll) — an MSVC-built helper will
# not start in the PE. This script uses ucrt64 and verifies the import names.
# MSYS2 artifacts go to a single shared <repo>\target\ucrt64\ so the (nearly
# identical) GTK dependency graph compiles once for all five helpers, and so
# they never thrash the per-helper MSVC/gvsbuild target dirs used for quick
# local look-tests.
#
# -Deploy copies the staged assets into winrx-creator's Programs cache, which
# the PEBakery script prefers over downloading a release — so a PE rebuild
# picks up the local binaries directly. Use CI/releases again by simply
# tagging as before (this script changes nothing about CI).
#
# Usage (from anywhere):
#   tools\build-local.ps1                 # build + stage into dist\
#   tools\build-local.ps1 -Deploy        # ...and copy into the winrx cache
#   tools\build-local.ps1 -SkipHelpers   # MSVC binaries only

param(
    [switch]$Deploy,
    [switch]$SkipHelpers,
    [string]$CacheDir = "D:\winrx-creator\Workbench\winrx-creator\Programs\StartPE"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $repo "dist"
$bash = "C:\msys64\usr\bin\bash.exe"

$helpers = @(
    @{ Dir = "helpers/sysinfo-gtk";    Asset = "SystemInfo.exe" },
    @{ Dir = "helpers/run-gtk";        Asset = "RunBox.exe" },
    @{ Dir = "helpers/settings-gtk";   Asset = "Settings.exe" },
    @{ Dir = "helpers/start-menu-gtk"; Asset = "StartMenu.exe" },
    @{ Dir = "helpers/network-gtk";    Asset = "Network.exe" }
)

# --- MSVC workspace ---------------------------------------------------------
Write-Host "== MSVC workspace (startpe.exe, startpe_loader.dll, syslaunch.exe)" -ForegroundColor Cyan
Push-Location $repo
try { cargo build --release --workspace; if ($LASTEXITCODE) { throw "MSVC build failed" } }
finally { Pop-Location }

New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item (Join-Path $repo "target\release\startpe.exe")         (Join-Path $dist "startpe.exe")
Copy-Item (Join-Path $repo "target\release\startpe_loader.dll")  (Join-Path $dist "startpe_loader.dll")
Copy-Item (Join-Path $repo "target\release\syslaunch.exe")       (Join-Path $dist "syslaunch.exe")

# --- GTK helpers via MSYS2 ucrt64 ------------------------------------------
if (-not $SkipHelpers) {
    if (-not (Test-Path $bash)) {
        throw "MSYS2 not found at C:\msys64. Install it and run:`n  pacman -S --needed mingw-w64-ucrt-x86_64-rust mingw-w64-ucrt-x86_64-pkgconf mingw-w64-ucrt-x86_64-gcc mingw-w64-ucrt-x86_64-gtk4 mingw-w64-ucrt-x86_64-libadwaita"
    }
    $repoUnix = ($repo -replace '\\', '/') -replace '^([A-Za-z]):', { "/$($_.Groups[1].Value.ToLower())" }

    foreach ($h in $helpers) {
        Write-Host "== ucrt64: $($h.Dir) -> $($h.Asset)" -ForegroundColor Cyan
        $env:MSYSTEM = "UCRT64"
        # Shared target dir: the GTK dep graph builds once, all helpers reuse it.
        & $bash -lc "cd '$repoUnix/$($h.Dir)' && CARGO_TARGET_DIR='$repoUnix/target/ucrt64' cargo build --release"
        if ($LASTEXITCODE) { throw "ucrt64 build failed: $($h.Dir)" }

        $exe = Join-Path $repo "target\ucrt64\release\$($h.Asset)"
        if (-not (Test-Path $exe)) { throw "expected output missing: $exe" }

        # Sanity: a PE-compatible helper imports the MSYS2 DLL names.
        $imports = & $bash -lc "objdump -p '$repoUnix/target/ucrt64/release/$($h.Asset)' | grep 'DLL Name' | grep -i gtk" 2>$null
        if ($imports -notmatch 'libgtk-4-1\.dll') {
            throw "$($h.Asset) does not import libgtk-4-1.dll (got: $imports) — wrong toolchain?"
        }
        Copy-Item $exe (Join-Path $dist $h.Asset)
    }
}

# --- summary / deploy -------------------------------------------------------
Write-Host "`n== staged in $dist" -ForegroundColor Green
Get-ChildItem $dist | Format-Table Name, Length, LastWriteTime -AutoSize | Out-Host

if ($Deploy) {
    if (-not (Test-Path $CacheDir)) { New-Item -ItemType Directory -Force $CacheDir | Out-Null }
    Copy-Item (Join-Path $dist "*") $CacheDir -Force
    Write-Host "== deployed to $CacheDir (PEBakery uses these instead of downloading)" -ForegroundColor Green
}
