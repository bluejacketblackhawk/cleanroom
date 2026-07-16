<#
.SYNOPSIS
    Stage the real ort/onnxruntime DirectML.dll so it ships next to the app exe.

.DESCRIPTION
    anvil-ai drives DeepFilterNet3 (Standard/Master) on `ort` built with the `directml`
    feature (crates/anvil-ai/Cargo.toml, Windows target). That pulls a DirectML-flavored
    onnxruntime whose provider has a LOAD-TIME dependency on DirectML.dll — the app fails to
    even start without it ("The code execution cannot proceed because directml.dll was not
    found."). C:\Windows\System32\DirectML.dll exists but is an older/incompatible version,
    so we must ship ort's own copy.

    At `cargo build` time ort downloads that runtime into a per-user pyke cache
    (%LOCALAPPDATA%\ort.pyke.io\dfbin\x86_64-pc-windows-msvc\<hash>\DirectML.dll — a real
    ~18 MB file) and drops a SYMLINK at <workspace>/target/release/DirectML.dll pointing at
    it. The Tauri bundler does not follow that symlink and it is not in bundle.resources, so
    the NSIS installer and portable zip shipped WITHOUT DirectML.dll and crashed on launch on
    every machine (the build dir resolves the symlink, which is why CLI/headless tests missed
    it). The exe loads DirectML.dll from its OWN directory, so the DLL must sit at the exe
    root — not in a subfolder.

    This script resolves the REAL DirectML.dll and copies it to
    vendor/ort/windows-x86_64/DirectML.dll, verifying it is the real ~18 MB file and never a
    ~147-byte symlink stub. tauri.conf.json's bundle.resources maps that path to the exe root
    ("./") and package-portable.mjs copies it next to the exe in the zip. It is wired to run
    from Tauri's `beforeBundleCommand` (after compile populates the cache, before bundling).

    Resolution order (robust):
      1. newest %LOCALAPPDATA%\ort.pyke.io\dfbin\x86_64-pc-windows-msvc\*\DirectML.dll
         (the real download cache — preferred; newest wins if a toolchain bump makes a new
         <hash> dir),
      2. fall back to the symlink target of <workspace>/target/release/DirectML.dll (and the
         per-crate target as a secondary), resolved to the real file.

.PARAMETER Force
    Re-copy even if an identically-sized DirectML.dll is already staged.

.EXAMPLE
    pwsh -File scripts/stage-directml.ps1
#>
[CmdletBinding()]
param(
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repoRoot = Split-Path -Parent $PSScriptRoot
$destDir = Join-Path $repoRoot 'vendor\ort\windows-x86_64'
$destDll = Join-Path $destDir 'DirectML.dll'

# A real DirectML.dll is ~18 MB. A Windows symlink reparse point reads as ~147 bytes, so any
# candidate at or below 1 MB is a stub we must reject — copying it would ship a broken app.
$minBytes = 1MB

function Get-LinkTarget($item) {
    # Reads the reparse target without tripping StrictMode when the member is absent.
    $prop = $item.PSObject.Properties['Target']
    if ($prop) { return $prop.Value }
    return $null
}

$candidates = New-Object System.Collections.Generic.List[System.IO.FileInfo]

# 1. The pyke download cache holds the real file. Take the NEWEST so a fresh onnxruntime
#    pull (new <hash> dir) is preferred over a stale one.
$cacheGlob = Join-Path $env:LOCALAPPDATA 'ort.pyke.io\dfbin\x86_64-pc-windows-msvc\*\DirectML.dll'
$cacheHits = @(Get-ChildItem -Path $cacheGlob -File -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending)
foreach ($hit in $cacheHits) { $candidates.Add($hit) }

# 2. Fall back to the build-output symlink and resolve it to the real file. Check the
#    workspace-root target first, then the per-crate target.
foreach ($rel in @('target\release\DirectML.dll',
                   'apps\desktop\src-tauri\target\release\DirectML.dll')) {
    $linkPath = Join-Path $repoRoot $rel
    if (-not (Test-Path -LiteralPath $linkPath)) { continue }
    $linkItem = Get-Item -LiteralPath $linkPath -Force
    $target = Get-LinkTarget $linkItem
    $resolved = $linkPath
    if ($target) {
        # Symlink targets may be relative to the link's own directory.
        if ([System.IO.Path]::IsPathRooted($target)) {
            $resolved = $target
        } else {
            $resolved = Join-Path (Split-Path -Parent $linkPath) $target
        }
    }
    if (Test-Path -LiteralPath $resolved) {
        $candidates.Add((Get-Item -LiteralPath $resolved -Force))
    }
}

# First candidate that is a REAL file (> 1 MB), never a symlink stub.
$real = $candidates | Where-Object { $_.Length -gt $minBytes } | Select-Object -First 1
if (-not $real) {
    throw @"
Could not locate a real ort DirectML.dll (> 1 MB).
Searched:
  cache glob:      $cacheGlob
  build symlink:   <repo>\target\release\DirectML.dll (and the per-crate target)
ort populates its pyke cache during `cargo build`, so a build must run first. If you only see
a ~147-byte file, that is the unresolved symlink: run `cargo build --release -p anvil-desktop`
and retry. This script is meant to run from Tauri's beforeBundleCommand (after compile).
"@
}

if (-not $Force -and (Test-Path -LiteralPath $destDll)) {
    $existing = (Get-Item -LiteralPath $destDll -Force).Length
    if ($existing -eq $real.Length) {
        Write-Host "DirectML.dll already staged ($existing bytes) at $destDll" -ForegroundColor Green
        exit 0
    }
}

New-Item -ItemType Directory -Force -Path $destDir | Out-Null
Copy-Item -LiteralPath $real.FullName -Destination $destDll -Force

$stagedLen = (Get-Item -LiteralPath $destDll -Force).Length
if ($stagedLen -le $minBytes) {
    throw "Staged DirectML.dll is only $stagedLen bytes - expected the real ~18 MB file, not a symlink stub. Aborting so we never ship a broken bundle."
}

Write-Host "Staged real DirectML.dll" -ForegroundColor Green
Write-Host "  from: $($real.FullName) ($($real.Length) bytes)"
Write-Host "  to:   $destDll ($stagedLen bytes)"
Write-Host "  -> bundle.resources maps vendor/ort/windows-x86_64/* to the exe root; package-portable.mjs copies it into the zip."
