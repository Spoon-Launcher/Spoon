# scripts/build-windows.ps1 - build Spoon natively on Windows and package it
# into dist/:
#
#   Spoon-Launcher-Windows-x86_64.exe (+ .zip)
#
# Uses the host MSVC toolchain, so it needs Rust (rustup) and the Visual
# Studio Build Tools. The asset names match the ones the sibling build
# scripts (build-linux.sh, build-macos.sh) produce and the updater downloads.
$ErrorActionPreference = 'Stop'

Set-Location (Join-Path $PSScriptRoot '..')
$dist = 'dist'
$target = 'x86_64-pc-windows-msvc'

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Command $($Arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo not found. Install Rust from https://www.rust-lang.org/tools/install'
}

Write-Host "`n=== Building Windows (x86_64) ===`n"
Invoke-Checked rustup @('target', 'add', $target)
Invoke-Checked cargo @('build', '--release', '-p', 'spoon-gui', '--target', $target)

$version = ((cargo pkgid -p spoon-gui) -split '#')[-1]
Write-Host "`n=== Packaging v$version into $dist/ ===`n"
New-Item -ItemType Directory -Force $dist | Out-Null

$exe = Join-Path $dist 'Spoon-Launcher-Windows-x86_64.exe'
$zip = Join-Path $dist 'Spoon-Launcher-Windows-x86_64.zip'
Copy-Item "target\$target\release\spoon-gui.exe" $exe -Force
Compress-Archive -Path $exe -DestinationPath $zip -Force

Write-Host "`n=== Done ==="
Get-ChildItem $dist -Filter 'Spoon-Launcher-Windows-*' | Format-Table Name, Length
