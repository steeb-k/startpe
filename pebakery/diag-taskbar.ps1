# SPDX-License-Identifier: GPL-3.0-or-later
# StartPE PE taskbar-feasibility diagnostic.
#
# Run this INSIDE the booted PE (an elevated PowerShell, before/without Explorer
# as shell). It answers the one question that decides the path to a working
# Explorer desktop on Win11 24H2/25H2 PE: can the modern taskbar's XAML CBS
# dependency packages be registered in this image?
#
#   powershell -ExecutionPolicy Bypass -File X:\diag-taskbar.ps1 > X:\diag-taskbar.txt 2>&1
#
# Paste the resulting X:\diag-taskbar.txt back.

$ErrorActionPreference = 'Continue'
function Section($t) { Write-Host "`n==== $t ====" }

Section "Environment"
Write-Host "whoami        : $(whoami)"
Write-Host "OS build      : $([System.Environment]::OSVersion.Version)"
Write-Host "SystemRoot    : $env:SystemRoot"
Write-Host "PSVersion     : $($PSVersionTable.PSVersion)"

Section "CBS / XAML package folders on disk"
$roots = @(
    "$env:SystemRoot\SystemApps",
    "$env:ProgramFiles\WindowsApps",
    "$env:SystemRoot\WinSxS"
)
$patterns = @(
    "MicrosoftWindows.Client.CBS*",
    "MicrosoftWindows.Client.Core*",
    "Microsoft.UI.Xaml.CBS*",
    "MicrosoftWindows.Client.FileExp*"
)
foreach ($r in $roots) {
    if (Test-Path $r) {
        foreach ($p in $patterns) {
            Get-ChildItem -Path $r -Filter $p -Directory -ErrorAction SilentlyContinue |
                ForEach-Object { Write-Host ("FOUND  {0}" -f $_.FullName) }
        }
    } else {
        Write-Host "MISSING ROOT  $r"
    }
}

Section "AppX subsystem availability"
Write-Host "Get-Command Get-AppxPackage : $([bool](Get-Command Get-AppxPackage -ErrorAction SilentlyContinue))"
Write-Host "Get-Command Add-AppxPackage : $([bool](Get-Command Add-AppxPackage -ErrorAction SilentlyContinue))"
foreach ($svc in 'AppXSvc','ClipSVC','StateRepository') {
    $s = Get-Service -Name $svc -ErrorAction SilentlyContinue
    if ($s) { Write-Host ("Service {0,-16}: {1}" -f $svc, $s.Status) }
    else    { Write-Host ("Service {0,-16}: <absent>" -f $svc) }
}

Section "Try: enumerate registered packages"
try {
    $pkgs = Get-AppxPackage -ErrorAction Stop
    Write-Host "Get-AppxPackage OK, count = $($pkgs.Count)"
} catch {
    Write-Host "Get-AppxPackage FAILED: $($_.Exception.Message)"
}

Section "Try: register one CBS package (the decisive test)"
$cbs = Get-ChildItem "$env:SystemRoot\SystemApps" -Filter "MicrosoftWindows.Client.CBS*" -Directory -ErrorAction SilentlyContinue | Select-Object -First 1
if ($cbs) {
    $manifest = Join-Path $cbs.FullName 'AppxManifest.xml'
    Write-Host "Manifest: $manifest (exists=$(Test-Path $manifest))"
    try {
        Add-AppxPackage -DisableDevelopmentMode -Register $manifest -ErrorAction Stop
        Write-Host "Add-AppxPackage -Register : SUCCESS  <-- registration works in this PE"
    } catch {
        Write-Host "Add-AppxPackage -Register : FAILED"
        Write-Host "  $($_.Exception.Message)"
    }
} else {
    Write-Host "No MicrosoftWindows.Client.CBS folder under SystemApps -- package not in image."
}

Section "Done"
