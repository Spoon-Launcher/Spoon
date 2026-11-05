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

# Optional Authenticode signing: an unsigned binary has no SmartScreen or
# Defender reputation, which is the main reason fresh Spoon builds get
# flagged as false positives. With a code-signing certificate configured,
# every packaged exe is signed here:
#   $env:SPOON_SIGN_PFX           = path to the .pfx certificate file
#   $env:SPOON_SIGN_PFX_PASSWORD  = its password (optional for password-less pfx)
# signtool.exe ships with the Windows SDK / Visual Studio Build Tools.
if ($env:SPOON_SIGN_PFX) {
    Write-Host "`n=== Signing $exe ===`n"
    $signtool = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if (-not $signtool) {
        throw 'signtool.exe not found. Install the Windows SDK signing tools or add signtool.exe to PATH.'
    }
    $signArgs = @('sign', '/fd', 'sha256', '/tr', 'http://timestamp.digicert.com', '/td', 'sha256', '/f', $env:SPOON_SIGN_PFX)
    if ($env:SPOON_SIGN_PFX_PASSWORD) {
        $signArgs += @('/p', $env:SPOON_SIGN_PFX_PASSWORD)
    }
    Invoke-Checked $signtool.Source ($signArgs + $exe)
}

Compress-Archive -Path $exe -DestinationPath $zip -Force

Write-Host "`n=== Done ==="
Get-ChildItem $dist -Filter 'Spoon-Launcher-Windows-*' | Format-Table Name, Length
