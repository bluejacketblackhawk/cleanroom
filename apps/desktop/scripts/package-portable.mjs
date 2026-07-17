#!/usr/bin/env node
// Builds Cleanroom's portable Windows distribution (05 §M5 lane A: "NSIS installer + portable
// zip"). Tauri's own bundler only produces the NSIS installer (`tauri.conf.json`'s
// `bundle.targets`) - there's no first-party "portable" bundle target, and there doesn't
// need to be one: a Tauri app is a single self-contained exe (WebView2 is OS-provided).
// "Portable" is that exe plus the bundled sidecars, zipped, with no installer and no
// registry writes.
//
// The sidecars are NOT fetched at first use - the engine sidecar managers never download
// anything (airplane-mode, ADR-005). They must ship IN the distribution: this script copies
// the provisioned `vendor/{ffmpeg,whisper,sherpa}` binaries and `vendor/models` into the
// zip in the exact `ffmpeg/ whisper/ sherpa/ models/` layout next to the exe that the NSIS
// installer produces from `tauri.conf.json`'s `bundle.resources`, so a clean unzip resolves
// them exe-relative with no env var. Run the `scripts/fetch-*.ps1` provisioners first (CI's
// release workflow does); this script fails loudly if `vendor/` is missing.
//
// Run AFTER `npm run tauri build` (or a bare `cargo build --release -p anvil-desktop`) has
// produced `<workspace>/target/release/<bin>.exe`. `apps/desktop/src-tauri` is a Cargo
// workspace member (see the root `Cargo.toml`'s `[workspace] members`), so the build
// lands in the *workspace root's* `target/`, not a per-crate one — this looks for both,
// preferring the workspace-root location, so the script still works if that ever changes.
// Writes `<release>/bundle/portable/Cleanroom-<version>-portable-x64.zip`, alongside where the
// NSIS bundler drops its own `bundle/nsis/*.exe`.
//
// Windows-only (matches the rest of M5 — win-arm64/macOS portable builds are separate
// milestones): shells out to PowerShell's `Compress-Archive` rather than pulling in a zip
// npm dependency, since this script only ever runs on the platform it's packaging for.

import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const desktopDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const srcTauriDir = path.join(desktopDir, "src-tauri");
const workspaceRoot = path.join(desktopDir, "..", "..");

const conf = JSON.parse(readFileSync(path.join(srcTauriDir, "tauri.conf.json"), "utf8"));
const version = conf.version;
const productName = conf.productName ?? "Cleanroom";

// Binary filename: Tauri's `mainBinaryName` if set, else the Cargo package name (from
// `src-tauri/Cargo.toml`'s `[package] name`, since `version.workspace = true` etc. don't
// tell us the name — we still read it directly rather than hardcoding "anvil-desktop" so
// this script doesn't silently drift from the Cargo manifest).
function cargoPackageName() {
  const toml = readFileSync(path.join(srcTauriDir, "Cargo.toml"), "utf8");
  const match = toml.match(/^\s*name\s*=\s*"([^"]+)"/m);
  if (!match) throw new Error("could not find [package] name in src-tauri/Cargo.toml");
  return match[1];
}
const binName = conf.mainBinaryName ?? cargoPackageName();

const candidateReleaseDirs = [
  path.join(workspaceRoot, "target", "release"),
  path.join(srcTauriDir, "target", "release"),
];
const releaseDir = candidateReleaseDirs.find((dir) =>
  existsSync(path.join(dir, `${binName}.exe`)),
);

if (!releaseDir) {
  console.error(
    `Expected a release build at ${binName}.exe under one of:\n` +
      candidateReleaseDirs.map((d) => `  ${d}`).join("\n") +
      `\nbut it's not there. Run "cargo build --release -p ${binName}" (or "npm run tauri build") first.`,
  );
  process.exit(1);
}
const exePath = path.join(releaseDir, `${binName}.exe`);

const outDir = path.join(releaseDir, "bundle", "portable");
mkdirSync(outDir, { recursive: true });

// Stage the portable payload in its own folder so the zip's top level is clean (just the
// exe + these two files, not an absolute-path mess) rather than zipping the exe alone.
const stageDir = path.join(outDir, "stage");
rmSync(stageDir, { recursive: true, force: true });
mkdirSync(stageDir, { recursive: true });

const licensePath = path.join(desktopDir, "..", "..", "LICENSE");
const readmeText = `Cleanroom ${version} — portable build
${"=".repeat(30)}

This is the no-install version: unzip anywhere and run ${binName}.exe. It writes only to
your per-user config/cache folders (never Program Files, never the registry) — the same
places the installed version uses — so you can switch between the two freely.

100% local. No account, no cloud, no telemetry. Airplane mode works.

Uninstalling: delete this folder. There's nothing else to clean up unless you turned on
Settings -> Integration (Explorer context menu / autostart) from *this* copy, in which
case run "${binName}.exe --uninstall-cleanup" first to remove those registry entries.
`;

copyFileSync(exePath, path.join(stageDir, `${binName}.exe`));
writeFileSync(path.join(stageDir, "README-portable.txt"), readmeText, "utf8");
if (existsSync(licensePath)) {
  copyFileSync(licensePath, path.join(stageDir, "LICENSE"));
}

// Bundled resources (`tauri.conf.json`'s `bundle.resources` — today just the onboarding
// demo file) — Tauri's runtime `resolveResource()` looks for these in a `resources/`
// folder *next to the exe* on Windows (confirmed against `cargo build`'s own staged
// output at `<release>/resources/`), the exact layout the NSIS installer also produces
// under `$INSTDIR`. Without this copy, the portable build's onboarding "Try the demo
// file" button would silently have nothing to open.
const resourcesSrc = path.join(releaseDir, "resources");
if (existsSync(resourcesSrc)) {
  cpSync(resourcesSrc, path.join(stageDir, "resources"), { recursive: true });
}

// Bundled sidecars + diarization models. These are provisioned into `<repo>/vendor` by the
// scripts/fetch-*.ps1 pins and are copied here into the SAME `ffmpeg/ whisper/ sherpa/
// models/` folders next to the exe that tauri.conf.json's `bundle.resources` lays down for
// the NSIS install - so the two distributions have an identical, exe-relative sidecar layout
// that FfmpegSidecar/WhisperSidecar/DiarizeSidecar::locate() (and the models dir) resolve
// with no env var. Sourced from `vendor` (the provisioning source of truth) rather than the
// build's staged `resources/`, so this holds even for a bare `cargo build --release`.
const vendorDir = path.join(workspaceRoot, "vendor");
const sidecars = [
  { from: path.join(vendorDir, "ffmpeg", "windows-x86_64"), to: "ffmpeg" },
  { from: path.join(vendorDir, "whisper", "windows-x86_64"), to: "whisper" },
  { from: path.join(vendorDir, "sherpa", "windows-x86_64"), to: "sherpa" },
  { from: path.join(vendorDir, "models"), to: "models" },
];
const missing = sidecars.filter((s) => !existsSync(s.from)).map((s) => s.from);
if (missing.length > 0) {
  console.error(
    "Cannot build the portable zip: bundled sidecars are not provisioned.\n" +
      "Missing:\n" +
      missing.map((d) => `  ${d}`).join("\n") +
      "\nRun the provisioners first (the release workflow does this automatically):\n" +
      "  pwsh scripts/fetch-ffmpeg.ps1\n" +
      "  pwsh scripts/fetch-whisper.ps1\n" +
      "  pwsh scripts/fetch-sherpa.ps1",
  );
  process.exit(1);
}
for (const { from, to } of sidecars) {
  cpSync(from, path.join(stageDir, to), { recursive: true });
}

// DirectML.dll. Unlike the sidecars above (which live in their own subfolders), ort loads
// DirectML.dll from the app exe's OWN directory, so it must sit at the zip root next to the
// exe — exactly where tauri.conf.json's bundle.resources maps it for the NSIS install ("./").
// It is staged into vendor/ort by scripts/stage-directml.ps1 (Tauri's beforeBundleCommand
// runs that during `tauri build`; package:win runs that build before this packager). Without
// it, the portable app dies on launch with a Windows loader error, so fail loudly if absent.
const directmlSrc = path.join(vendorDir, "ort", "windows-x86_64", "DirectML.dll");
if (!existsSync(directmlSrc)) {
  console.error(
    `Cannot build the portable zip: DirectML.dll is not staged at\n  ${directmlSrc}\n` +
      "It is produced by a build (ort's pyke cache) + scripts/stage-directml.ps1. Run\n" +
      '  npm run package:win\n' +
      "(which builds, stages, then packages) rather than package:portable in isolation, or\n" +
      "run a `tauri build` first so beforeBundleCommand stages it.",
  );
  process.exit(1);
}
copyFileSync(directmlSrc, path.join(stageDir, "DirectML.dll"));

const zipName = `${productName}-${version}-portable-x64.zip`;
const zipPath = path.join(outDir, zipName);
rmSync(zipPath, { force: true });

// `Compress-Archive -Path <dir>\*` zips the stage folder's *contents* at the zip root
// (not the stage folder itself), which is the layout the README above describes.
execFileSync(
  "powershell.exe",
  [
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    `Compress-Archive -Path '${stageDir}\\*' -DestinationPath '${zipPath}' -CompressionLevel Optimal -Force`,
  ],
  { stdio: "inherit" },
);

rmSync(stageDir, { recursive: true, force: true });

if (!existsSync(zipPath)) {
  console.error(`Compress-Archive did not produce ${zipPath}`);
  process.exit(1);
}
console.log(`Wrote ${zipPath}`);
