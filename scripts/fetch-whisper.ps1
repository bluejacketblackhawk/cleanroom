<#
.SYNOPSIS
    Provision the pinned MIT whisper.cpp CLI sidecar, reproducibly and verifiably.

.DESCRIPTION
    Cleanroom is MIT and runs whisper.cpp as a SIDECAR PROCESS (never linked). This script fetches
    exactly the build named in scripts/whisper-pin.json and refuses to install anything else:

      1. downloads the archive from the pinned immutable release-asset URL,
      2. verifies the archive's sha256 against the pin,
      3. extracts whisper-cli.exe + every runtime DLL it needs and verifies each one's sha256,
      4. copies the MIT licence text next to the binaries (redistribution duty),
      5. runs `whisper-cli --help` to prove the binary + its DLLs actually load.

    The result lands in vendor/whisper/windows-x86_64/, which the app resolves exe-relative
    (WhisperSidecar::locate() looks in a `whisper/` folder next to the executable) and which
    packaging copies next to the app.

.PARAMETER Force
    Re-download and re-verify even if already installed and correct.

.EXAMPLE
    pwsh -File scripts/fetch-whisper.ps1
    $env:CLEANROOM_WHISPER = "$PWD\vendor\whisper\windows-x86_64\whisper-cli.exe"
#>
[CmdletBinding()]
param(
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# PowerShell 5.1's Invoke-WebRequest is ~100x slower with the progress bar rendering; off.
$ProgressPreference = 'SilentlyContinue'

$repoRoot = Split-Path -Parent $PSScriptRoot
$pinPath = Join-Path $PSScriptRoot 'whisper-pin.json'
# Pins are now keyed by target; the mac entries live under .targets.macos-* and are built by
# scripts/build-whisper-macos.sh. This Windows provisioner reads the windows-x86_64 entry (whose
# shape is unchanged from the old flat pin, so nothing below needs to change).
$pinFile = Get-Content -Raw -Path $pinPath | ConvertFrom-Json
$pin = $pinFile.targets.'windows-x86_64'

$destDir = Join-Path $repoRoot "vendor\whisper\$($pin.target)"
$cacheDir = Join-Path $repoRoot '.cache\whisper'
$archive = Join-Path $cacheDir (Split-Path -Leaf ([Uri]$pin.source_url).AbsolutePath)

function Get-Sha256([string]$Path) {
    (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLower()
}

function Assert-Sha256([string]$Path, [string]$Expected, [string]$What) {
    $actual = Get-Sha256 $Path
    if ($actual -ne $Expected.ToLower()) {
        Remove-Item -Force $Path -ErrorAction SilentlyContinue
        throw "$What sha256 mismatch.`n  expected: $Expected`n  actual:   $actual`nRefusing to install an unverified whisper.cpp. If the upstream asset genuinely changed, re-audit and update scripts/whisper-pin.json."
    }
    Write-Host "  ok  $What sha256 $actual" -ForegroundColor DarkGreen
}

# Already provisioned and correct? (verify the primary binary only - the cheap check.)
$destExe = Join-Path $destDir 'whisper-cli.exe'
if (-not $Force -and (Test-Path $destExe) -and ((Get-Sha256 $destExe) -eq $pin.binary_sha256)) {
    Write-Host "whisper.cpp $($pin.version) already provisioned at $destExe" -ForegroundColor Green
    Write-Host "  set CLEANROOM_WHISPER=$destExe"
    exit 0
}

Write-Host "Provisioning MIT whisper.cpp $($pin.version) for $($pin.target)" -ForegroundColor Cyan
Write-Host "  source:  $($pin.source_url)"
Write-Host "  licence: $($pin.license) ($($pin.license_holder))"

New-Item -ItemType Directory -Force -Path $cacheDir, $destDir | Out-Null

# 1 + 2. Download (cached) and verify the archive.
if ($Force -or -not (Test-Path $archive) -or ((Get-Sha256 $archive) -ne $pin.archive_sha256)) {
    Write-Host "Downloading $([math]::Round($pin.archive_bytes / 1MB, 1)) MB ..."
    Invoke-WebRequest -UseBasicParsing -Uri $pin.source_url -OutFile $archive -TimeoutSec 900
}
Assert-Sha256 $archive $pin.archive_sha256 'archive'

# 3. Extract every pinned member, verify each, copy into the vendor dir (flat).
$staging = Join-Path $cacheDir 'extract'
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
Expand-Archive -Path $archive -DestinationPath $staging -Force

foreach ($member in $pin.members) {
    $src = Join-Path $staging ($member.archive_path -replace '/', '\')
    if (-not (Test-Path $src)) { throw "archive member $($member.archive_path) not found in $archive" }
    Assert-Sha256 $src $member.sha256 $member.dest
    Copy-Item -Force $src (Join-Path $destDir $member.dest)
}
Remove-Item -Recurse -Force $staging

# 4. Ship the licence text next to the binaries (MIT redistribution duty).
$licenseSrc = Join-Path $PSScriptRoot "licenses\$($pin.license_file)"
if (-not (Test-Path $licenseSrc)) { throw "licence text $licenseSrc missing - an MIT binary must ship its notice" }
Copy-Item -Force $licenseSrc (Join-Path $destDir 'LICENSE.txt')

# 5. Prove it runs (exercises whisper-cli + whisper.dll + ggml*.dll loading).
# whisper-cli writes its "load_backend" line to stderr; under $ErrorActionPreference='Stop' a
# merged native stderr becomes a terminating NativeCommandError, so relax EAP for the call.
Write-Host "Verifying $destExe --help ..." -ForegroundColor Cyan
$prevEAP = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$help = (& $destExe --help 2>&1 | Out-String)
$code = $LASTEXITCODE
$ErrorActionPreference = $prevEAP
if ($code -ne 0 -and $help -notmatch 'usage|options') {
    throw "whisper-cli did not run cleanly (exit $code). Missing a runtime DLL?`n$help"
}
Write-Host "  ok  whisper-cli loaded its backend and printed usage" -ForegroundColor DarkGreen

Write-Host ""
Write-Host "Installed whisper.cpp $($pin.version) ($($pin.license))" -ForegroundColor Green
Write-Host "  binary:  $destExe"
Write-Host "  dir:     $destDir  (whisper-cli.exe + whisper.dll + ggml*.dll + LICENSE.txt)"
Write-Host ""
Write-Host "For dev, point Cleanroom at it:" -ForegroundColor Cyan
Write-Host "  `$env:CLEANROOM_WHISPER = `"$destExe`""
Write-Host ""
Write-Host "Packaging places this whole folder as 'whisper/' next to the app executable;"
Write-Host "WhisperSidecar::locate() resolves it there with no env var."
