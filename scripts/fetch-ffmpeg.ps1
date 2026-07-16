<#
.SYNOPSIS
    Provision the pinned LGPL ffmpeg sidecar, reproducibly and verifiably.

.DESCRIPTION
    ANVIL is MIT and ships ffmpeg as a SIDECAR PROCESS (never linked). The binary we
    redistribute must therefore be a GPL-free build. This script fetches exactly the build
    named in scripts/ffmpeg-pin.json and refuses to install anything else:

      1. downloads the archive from the pinned immutable release-asset URL,
      2. verifies the archive's sha256 against the pin,
      3. extracts ffmpeg.exe + the LGPL licence text,
      4. verifies the binary's sha256 against the pin (this is the hash the app enforces at
         run time -- anvil_media::sidecar::PINNED_FFMPEG_SHA256),
      5. runs `ffmpeg -version` and FAILS if the configure line enables any GPL or nonfree
         component (x264, x265, xvid, fdk-aac, rubberband, vidstab, frei0r, avisynth, ...).

    Step 5 is the licence gate. FFmpeg's own configure calls `die_license_disabled gpl` over
    EXTERNAL_LIBRARY_GPL_LIST, so a build that carries no `--enable-gpl` provably links none
    of those libraries. We re-check it here anyway, because "the file we shipped" and "the
    file we audited" being the same thing is the entire point.

.PARAMETER Force
    Re-download and re-verify even if the binary is already installed and correct.

.EXAMPLE
    pwsh -File scripts/fetch-ffmpeg.ps1
    $env:ANVIL_FFMPEG = "$PWD\vendor\ffmpeg\windows-x86_64\ffmpeg.exe"
#>
[CmdletBinding()]
param(
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# PowerShell 5.1's Invoke-WebRequest is ~100x slower with progress-bar rendering (relevant for
# a 145 MB download in CI); disabling it is a pure speed win.
$ProgressPreference = 'SilentlyContinue'

$repoRoot = Split-Path -Parent $PSScriptRoot
$pinPath = Join-Path $PSScriptRoot 'ffmpeg-pin.json'
# ffmpeg-pin.json is a map of <os>-<arch> targets (mirroring anvil_media::sidecar::FFMPEG_PINS);
# this script provisions the Windows sidecar. forbidden_configure_markers is shared at top level.
$pinFile = Get-Content -Raw -Path $pinPath | ConvertFrom-Json
$pin = $pinFile.targets.'windows-x86_64'

$destDir = Join-Path $repoRoot "vendor\ffmpeg\$($pin.target)"
$destExe = Join-Path $destDir 'ffmpeg.exe'
$destLicense = Join-Path $destDir 'LICENSE.txt'
$cacheDir = Join-Path $repoRoot '.cache\ffmpeg'
$archive = Join-Path $cacheDir (Split-Path -Leaf ([Uri]$pin.source_url).AbsolutePath)

function Get-Sha256([string]$Path) {
    (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLower()
}

function Assert-Sha256([string]$Path, [string]$Expected, [string]$What) {
    $actual = Get-Sha256 $Path
    if ($actual -ne $Expected.ToLower()) {
        Remove-Item -Force $Path -ErrorAction SilentlyContinue
        throw "$What sha256 mismatch.`n  expected: $Expected`n  actual:   $actual`nRefusing to install an unverified ffmpeg. If the upstream asset genuinely changed, re-audit the build and update scripts/ffmpeg-pin.json AND anvil_media::sidecar::FFMPEG_PIN together."
    }
    Write-Host "  ok  $What sha256 $actual" -ForegroundColor DarkGreen
}

# Already provisioned and correct? Nothing to do.
if (-not $Force -and (Test-Path $destExe) -and ((Get-Sha256 $destExe) -eq $pin.binary_sha256)) {
    Write-Host "ffmpeg $($pin.version) already provisioned at $destExe" -ForegroundColor Green
    Write-Host "  set ANVIL_FFMPEG=$destExe"
    exit 0
}

Write-Host "Provisioning LGPL ffmpeg $($pin.version) for $($pin.target)" -ForegroundColor Cyan
Write-Host "  source:  $($pin.source_url)"
Write-Host "  licence: $($pin.license)"

New-Item -ItemType Directory -Force -Path $cacheDir, $destDir | Out-Null

# 1 + 2. Download (cached) and verify the archive.
if ($Force -or -not (Test-Path $archive) -or ((Get-Sha256 $archive) -ne $pin.archive_sha256)) {
    Write-Host "Downloading $([math]::Round($pin.archive_bytes / 1MB, 1)) MB ..."
    Invoke-WebRequest -UseBasicParsing -Uri $pin.source_url -OutFile $archive -TimeoutSec 900
}
Assert-Sha256 $archive $pin.archive_sha256 'archive'

# 3. Extract just the binary and the licence text.
$staging = Join-Path $cacheDir 'extract'
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
Expand-Archive -Path $archive -DestinationPath $staging -Force

$srcExe = Join-Path $staging ($pin.archive_member -replace '/', '\')
$srcLicense = Join-Path $staging ($pin.license_file -replace '/', '\')
if (-not (Test-Path $srcExe)) { throw "archive member $($pin.archive_member) not found in $archive" }
if (-not (Test-Path $srcLicense)) { throw "licence file $($pin.license_file) not found in $archive -- an LGPL binary must ship its licence text" }

Copy-Item -Force $srcExe $destExe
Copy-Item -Force $srcLicense $destLicense
Remove-Item -Recurse -Force $staging

# 4. Verify the binary -- this is the hash the running app enforces.
Assert-Sha256 $destExe $pin.binary_sha256 'ffmpeg.exe'

# 5. The licence gate: no GPL / nonfree component may be enabled.
$versionOut = & $destExe -version 2>&1 | Out-String
$configureLine = ($versionOut -split "`n" | Where-Object { $_ -match '^configuration:' } | Select-Object -First 1)
if (-not $configureLine) { throw "$destExe printed no configure line" }

$enabled = [regex]::Matches($configureLine, '--enable-([a-z0-9_\-]+)') |
    ForEach-Object { $_.Groups[1].Value -replace '-', '_' }
$violations = @($enabled | Where-Object { $pinFile.forbidden_configure_markers -contains $_ })

if ($violations.Count -gt 0) {
    Remove-Item -Force $destExe
    throw "REFUSING THIS BUILD: its configure line enables GPL/nonfree components: $($violations -join ', ').`nANVIL is MIT and redistributes this binary; it must be an LGPL-only build."
}

Write-Host "  ok  configure line is GPL-free (no --enable-gpl, no --enable-nonfree)" -ForegroundColor DarkGreen
Write-Host ""
Write-Host "Installed $($pin.version) ($($pin.license))" -ForegroundColor Green
Write-Host "  binary:  $destExe"
Write-Host "  licence: $destLicense"
Write-Host ""
Write-Host "For dev, point ANVIL at it:" -ForegroundColor Cyan
Write-Host "  `$env:ANVIL_FFMPEG = `"$destExe`""
Write-Host ""
Write-Host "The installer must place this binary (and LICENSE.txt) next to the app executable;"
Write-Host "the app hash-checks it against FFMPEG_PIN on every launch and refuses a mismatch."
