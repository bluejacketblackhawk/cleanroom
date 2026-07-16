<#
.SYNOPSIS
    Provision the pinned sherpa-onnx speaker-diarization sidecar + its two default models.

.DESCRIPTION
    ANVIL is MIT and runs sherpa-onnx as a SIDECAR PROCESS (never linked). This script fetches
    exactly the build + models named in scripts/sherpa-pin.json and refuses anything else:

      1. downloads the sherpa 'shared' Windows archive, verifies its sha256,
      2. extracts sherpa-onnx-offline-speaker-diarization.exe + onnxruntime.dll (+ providers
         stub), verifying each one's sha256,
      3. ships the Apache-2.0 (sherpa) and MIT (onnxruntime) licence texts next to them,
      4. downloads + verifies the DEFAULT diarization models (segmentation + speaker embedding)
         so diarization works out of the box, and ships their licences/attribution,
      5. runs the diarization exe to prove it loads.

    Binaries land in vendor/sherpa/windows-x86_64/ (resolved exe-relative by
    DiarizeSidecar::locate() in a `sherpa/` folder next to the app); models land in
    vendor/models/ (resolved by the app's models dir next to the exe).

    The model sha256s here MUST equal crates/anvil-asr/src/model.rs KNOWN_DIARIZATION_MODELS.
    NOTE: TitaNet-small is CC-BY-4.0 - REDISTRIBUTABLE WITH ATTRIBUTION (see the shipped
    titanet-small-ATTRIBUTION.txt); it is the working default because Apache-2.0 CAM++ is
    degenerate on the shipped Windows onnxruntime (model.rs explains why).

.PARAMETER Force
    Re-download and re-verify even if already installed and correct.

.EXAMPLE
    pwsh -File scripts/fetch-sherpa.ps1
    $env:ANVIL_DIARIZE = "$PWD\vendor\sherpa\windows-x86_64\sherpa-onnx-offline-speaker-diarization.exe"
#>
[CmdletBinding()]
param(
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repoRoot = Split-Path -Parent $PSScriptRoot
$pinPath = Join-Path $PSScriptRoot 'sherpa-pin.json'
# Pins are now keyed by target; the mac entries live under .targets.macos-* and are provisioned
# by scripts/fetch-sherpa-macos.sh. This Windows provisioner reads the windows-x86_64 entry.
# The models[] array stays OS-independent (top level), so it is read from $pinFile, not $pin.
$pinFile = Get-Content -Raw -Path $pinPath | ConvertFrom-Json
$pin = $pinFile.targets.'windows-x86_64'

$binDir = Join-Path $repoRoot "vendor\sherpa\$($pin.target)"
$modelsDir = Join-Path $repoRoot 'vendor\models'
$cacheDir = Join-Path $repoRoot '.cache\sherpa'
$licenseDir = Join-Path $PSScriptRoot 'licenses'
# Use Windows bsdtar explicitly (handles .tar.bz2 and C:\ paths). Resolving a bare `tar` off
# PATH can pick up an MSYS/Git tar that treats `C:\...` as a remote host and fails.
$tarExe = Join-Path $env:SystemRoot 'System32\tar.exe'
if (-not (Test-Path $tarExe)) { $tarExe = 'tar' }

function Get-Sha256([string]$Path) {
    (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLower()
}

function Assert-Sha256([string]$Path, [string]$Expected, [string]$What) {
    $actual = Get-Sha256 $Path
    if ($actual -ne $Expected.ToLower()) {
        Remove-Item -Force $Path -ErrorAction SilentlyContinue
        throw "$What sha256 mismatch.`n  expected: $Expected`n  actual:   $actual`nRefusing to install an unverified sherpa-onnx asset. If upstream genuinely changed, re-audit and update scripts/sherpa-pin.json (and keep model hashes in sync with model.rs)."
    }
    Write-Host "  ok  $What sha256 $actual" -ForegroundColor DarkGreen
}

function Copy-License([string]$Name, [string]$DestFolder) {
    $src = Join-Path $licenseDir $Name
    if (-not (Test-Path $src)) { throw "licence text $src missing - a redistributed binary/model must ship its notice" }
    Copy-Item -Force $src (Join-Path $DestFolder $Name)
}

function Get-Cached([string]$Url) {
    $path = Join-Path $cacheDir (Split-Path -Leaf ([Uri]$Url).AbsolutePath)
    if ($Force -or -not (Test-Path $path)) {
        Write-Host "Downloading $(Split-Path -Leaf $path) ..."
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $path -TimeoutSec 900
    }
    $path
}

New-Item -ItemType Directory -Force -Path $cacheDir, $binDir, $modelsDir | Out-Null

# --- 1..3: the sherpa binary bundle -------------------------------------------------------
$b = $pin.binary
$destExe = Join-Path $binDir 'sherpa-onnx-offline-speaker-diarization.exe'
$binOk = (-not $Force) -and (Test-Path $destExe) -and ((Get-Sha256 $destExe) -eq $b.binary_sha256)

if (-not $binOk) {
    Write-Host "Provisioning sherpa-onnx $($pin.version) diarization binary for $($pin.target)" -ForegroundColor Cyan
    Write-Host "  source:  $($b.source_url)"
    Write-Host "  licence: $($b.license) ($($b.license_holder))"

    $archive = Get-Cached $b.source_url
    Assert-Sha256 $archive $b.archive_sha256 'sherpa archive'

    $staging = Join-Path $cacheDir 'extract'
    if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    & $tarExe -xf $archive -C $staging
    if ($LASTEXITCODE -ne 0) { throw "tar failed to extract $archive" }

    foreach ($member in $b.members) {
        $src = Join-Path $staging ($member.archive_path -replace '/', '\')
        if (-not (Test-Path $src)) { throw "archive member $($member.archive_path) not found in $archive" }
        Assert-Sha256 $src $member.sha256 $member.dest
        Copy-Item -Force $src (Join-Path $binDir $member.dest)
    }
    Remove-Item -Recurse -Force $staging

    foreach ($lf in $b.license_files) { Copy-License $lf $binDir }
} else {
    Write-Host "sherpa-onnx $($pin.version) binary already provisioned at $destExe" -ForegroundColor Green
}

# --- 4: the default diarization models (OS-independent — top-level models[], not per-target) --
foreach ($m in $pinFile.models) {
    $destOnnx = Join-Path $modelsDir $m.dest
    if (-not $Force -and (Test-Path $destOnnx) -and ((Get-Sha256 $destOnnx) -eq $m.onnx_sha256)) {
        Write-Host "model $($m.id) already provisioned at $destOnnx" -ForegroundColor Green
        continue
    }
    Write-Host "Provisioning $($m.kind) model $($m.id) ($($m.license))" -ForegroundColor Cyan
    $archive = Get-Cached $m.source_url

    if ($m.PSObject.Properties.Name -contains 'archive_member') {
        # Model ships inside a .tar.bz2 (e.g. pyannote segmentation).
        Assert-Sha256 $archive $m.archive_sha256 "$($m.id) archive"
        $staging = Join-Path $cacheDir "model-$($m.id)"
        if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
        New-Item -ItemType Directory -Force -Path $staging | Out-Null
        & $tarExe -xf $archive -C $staging
        if ($LASTEXITCODE -ne 0) { throw "tar failed to extract $archive" }
        $src = Join-Path $staging ($m.archive_member -replace '/', '\')
        if (-not (Test-Path $src)) { throw "model member $($m.archive_member) not found in $archive" }
        Assert-Sha256 $src $m.onnx_sha256 "$($m.dest)"
        Copy-Item -Force $src $destOnnx
        Remove-Item -Recurse -Force $staging
    } else {
        # Model is the bare .onnx (e.g. TitaNet).
        Assert-Sha256 $archive $m.onnx_sha256 "$($m.dest)"
        Copy-Item -Force $archive $destOnnx
    }
    Copy-License $m.license_file $modelsDir
    if ($m.PSObject.Properties.Name -contains 'license_extra_file') {
        Copy-License $m.license_extra_file $modelsDir
    }
}

# --- 5: prove the diarization exe loads ---------------------------------------------------
# sherpa prints usage via its own parser to stderr; relax EAP so the merged native stderr does
# not become a terminating NativeCommandError under $ErrorActionPreference='Stop'.
Write-Host "Verifying $destExe loads ..." -ForegroundColor Cyan
$prevEAP = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$usage = (& $destExe --help 2>&1 | Out-String)
$ErrorActionPreference = $prevEAP
if ($usage -notmatch 'diarization') {
    throw "sherpa diarization exe did not print usage. Missing a runtime DLL?`n$usage"
}
Write-Host "  ok  sherpa-onnx-offline-speaker-diarization loaded" -ForegroundColor DarkGreen

Write-Host ""
Write-Host "Installed sherpa-onnx $($pin.version) diarization ($($b.license) engine, MIT onnxruntime)" -ForegroundColor Green
Write-Host "  binary:  $destExe"
Write-Host "  models:  $modelsDir  (segmentation + embedding + licences)"
Write-Host ""
Write-Host "Packaging places the binary folder as 'sherpa/' and the models as 'models/' next to"
Write-Host "the app executable; DiarizeSidecar::locate() and the models dir resolve them env-free."
Write-Host ""
Write-Host "ATTRIBUTION: TitaNet-small is CC-BY-4.0 - the About screen must credit NVIDIA NeMo"
Write-Host "(see vendor/models/titanet-small-ATTRIBUTION.txt)." -ForegroundColor Yellow
