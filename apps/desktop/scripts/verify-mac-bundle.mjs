#!/usr/bin/env node
// verify-mac-bundle.mjs — post-bundle gate for the macOS `.app`. The Mac analogue of
// release.yml's "Verify DirectML.dll is bundled" step: it proves the built bundle actually carries
// every sidecar, in the exact layout the engine's `locate()` resolves, with the +x bit intact and
// each binary spawnable env-free (which is also the onnxruntime/dylib LOAD check — the trap that
// bit Windows). Exits NONZERO on the first category of failure so CI fails here, not on a user's Mac.
//
// WHY this exists as a script (not just the stage step): Tauri copies `bundle.resources` into
// `Contents/Resources/`, and that copy is where the Unix exec bit can be dropped (handoff/08-MAC.md
// §4) and where a glob could silently place a file one directory off. Only inspecting the finished
// `.app` catches those. It re-runs the same env-stripped spawn the app itself will do, so a missing
// dylib or a broken rpath fails here deterministically.
//
// CLI CONTRACT (stable — another lane owns .github/workflows; it just invokes this):
//   node apps/desktop/scripts/verify-mac-bundle.mjs <path-to-.app>
//   node apps/desktop/scripts/verify-mac-bundle.mjs --target <triple>   # locate the built .app
//   node apps/desktop/scripts/verify-mac-bundle.mjs                     # default: target/release/…
// Exit 0 = all hard checks passed. Exit 1 = at least one failed (details printed). Exit 2 = the
// `.app` could not be located (usage error). A SIGNED-but-tampered Mach-O is a hard failure
// (codesign --verify --strict, section [5]); an unsigned Mach-O is only reported, since this runs
// both before AND after signing. The signing-INDEPENDENT content-hash pin gate (the identity the
// engine enforces at run time) is verified by the mac-gated Rust integration tests, not re-derived
// here — see section [5] and ADR-012.
//
// It is arch-agnostic: point it at either per-arch `.app`. On an Apple-Silicon host the x86_64
// sidecars spawn transparently under Rosetta; the universal2 sherpa binary runs native either way.

import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const desktopDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const workspaceRoot = path.join(desktopDir, "..", "..");
const conf = JSON.parse(
  readFileSync(path.join(desktopDir, "src-tauri", "tauri.conf.json"), "utf8"),
);
const productName = conf.productName ?? "Cleanroom";

// --- locate the .app ---------------------------------------------------------------------------
// Deterministic: exactly `<productName>.app` per tauri.conf.json — never "the first .app in the
// dir", which once picked a stale pre-rename Cleanroom.app over Cleanroom.app and validated the wrong
// bundle. Strays next to the real one are noted; a dir holding ONLY foreign .apps is a hard error.
// An explicit <path-to-.app> argument still wins unchanged.
function resolveAppPath(argv) {
  const args = argv.slice(2);
  // explicit path to a .app
  const explicit = args.find((a) => a.endsWith(".app"));
  if (explicit) return path.resolve(explicit);

  // --target <triple>  |  --target=<triple>
  let triple = null;
  const eq = args.find((a) => a.startsWith("--target="));
  if (eq) triple = eq.slice("--target=".length);
  const flagIdx = args.indexOf("--target");
  if (flagIdx !== -1 && args[flagIdx + 1]) triple = args[flagIdx + 1];

  const bundleMacos = triple
    ? path.join(workspaceRoot, "target", triple, "release", "bundle", "macos")
    : path.join(workspaceRoot, "target", "release", "bundle", "macos");
  if (!existsSync(bundleMacos)) return null;
  const apps = readdirSync(bundleMacos).filter((f) => f.endsWith(".app"));
  const wanted = `${productName}.app`;
  if (apps.includes(wanted)) {
    const strays = apps.filter((a) => a !== wanted);
    if (strays.length)
      console.error(`verify-mac-bundle: note — ignoring stale bundle(s) next to ${wanted}: ${strays.join(", ")}`);
    return path.join(bundleMacos, wanted);
  }
  if (apps.length) {
    console.error(
      `verify-mac-bundle: ${wanted} (the configured productName) not found in ${bundleMacos}\n` +
        `  but the dir does contain: ${apps.join(", ")}\n` +
        `  Refusing to verify an arbitrary .app — it could be a stale pre-rename bundle.\n` +
        `  Delete the stray bundle(s), or pass the intended .app path explicitly.`,
    );
    process.exit(2);
  }
  return null;
}

const appPath = resolveAppPath(process.argv);
if (!appPath || !existsSync(appPath)) {
  console.error(
    "verify-mac-bundle: could not find a .app to verify.\n" +
      "  usage: node apps/desktop/scripts/verify-mac-bundle.mjs <path-to-.app>\n" +
      "     or: node apps/desktop/scripts/verify-mac-bundle.mjs --target <triple>",
  );
  process.exit(2);
}

const resources = path.join(appPath, "Contents", "Resources");
const macosDir = path.join(appPath, "Contents", "MacOS");
const infoPlist = path.join(appPath, "Contents", "Info.plist");

const failures = [];
const fail = (msg) => {
  failures.push(msg);
  console.log(`  FAIL  ${msg}`);
};
const pass = (msg) => console.log(`  ok    ${msg}`);
const section = (msg) => console.log(`\n${msg}`);

console.log(`Verifying ${appPath}`);

// --- 1. sidecar binaries: present, executable, spawnable env-free ------------------------------
// Each entry: the Resources-relative path the engine's locate() resolves, and a spawn recipe whose
// output proves the binary AND its dylibs loaded (env stripped = the real airplane/no-DYLD case).
const binaries = [
  {
    rel: "ffmpeg/ffmpeg",
    args: ["-version"],
    marker: /ffmpeg version/i,
    who: "anvil-media FfmpegSidecar::locate ../Resources/ffmpeg/ffmpeg",
  },
  {
    rel: "whisper/whisper-cli",
    args: ["--help"],
    marker: /whisper|usage/i,
    who: "anvil-asr WhisperSidecar::locate ../Resources/whisper/whisper-cli",
  },
  {
    rel: "sherpa/bin/sherpa-onnx-offline-speaker-diarization",
    args: ["--help"],
    marker: /diariz|speaker/i,
    who: "anvil-asr DiarizeSidecar::locate ../Resources/sherpa/bin/<exe> (rpath @loader_path/../lib → onnxruntime)",
  },
];

section("[1] sidecar executables — present, +x, spawnable env-free");
for (const b of binaries) {
  const p = path.join(resources, b.rel);
  if (!existsSync(p)) {
    fail(`${b.rel} missing (${b.who})`);
    continue;
  }
  const mode = statSync(p).mode;
  if (!(mode & 0o111)) {
    fail(`${b.rel} is present but NOT executable (mode ${(mode & 0o777).toString(8)}) — Tauri dropped the +x bit`);
    continue;
  }
  // env stripped: no PATH, no DYLD_*, no HOME — exactly the shipped runtime. Absolute path, so no
  // PATH is needed. A missing/misplaced dylib surfaces here as a dyld abort, not a silent pass.
  const r = spawnSync(p, b.args, { env: {}, encoding: "utf8", timeout: 30000 });
  const out = `${r.stdout || ""}${r.stderr || ""}`;
  if (r.error) {
    fail(`${b.rel} could not spawn env-free: ${r.error.message}`);
  } else if (/dyld|Library not loaded|image not found/i.test(out) && !b.marker.test(out)) {
    fail(`${b.rel} spawned but dyld could not resolve its libraries:\n      ${out.split("\n").slice(0, 4).join("\n      ")}`);
  } else if (!b.marker.test(out)) {
    fail(`${b.rel} spawned but produced no expected output (${b.marker}):\n      ${out.split("\n").slice(0, 3).join("\n      ")}`);
  } else {
    pass(`${b.rel} — +x, runs env-free`);
  }
}

// --- 2. dylibs + models present at the exact locate() paths ------------------------------------
section("[2] bundled libraries + models at the locate() paths");
const requiredFiles = [
  // the shared onnxruntime — serves the sherpa sidecar AND (Intel) in-process ort (ADR-012)
  "sherpa/lib/libonnxruntime.1.17.1.dylib",
  // whisper-cli's dylibs (rpath @loader_path — resolved next to the binary)
  "whisper/libwhisper.1.dylib",
  "whisper/libggml.0.dylib",
  "whisper/libggml-base.0.dylib",
  "whisper/libggml-cpu.0.dylib",
  "whisper/libggml-metal.0.dylib",
  "whisper/libggml-blas.0.dylib",
  // the two diarization models bundled so diarization works out of the box
  "models/sherpa-onnx-pyannote-segmentation-3-0.onnx",
  "models/nemo_en_titanet_small.onnx",
];
for (const rel of requiredFiles) {
  if (existsSync(path.join(resources, rel))) pass(rel);
  else fail(`${rel} missing from Contents/Resources/`);
}

// --- 3. onboarding demo file -------------------------------------------------------------------
section("[3] onboarding demo file");
const demoRel = "resources/demo/bad-recording-example.wav";
if (existsSync(path.join(resources, demoRel))) pass(demoRel);
else fail(`${demoRel} missing — the onboarding "Try the demo file" button would open nothing`);

// --- 4. Info.plist: associations, min system version, mic usage string -------------------------
section("[4] Info.plist — file associations, min system version, mic usage string");
let plist = null;
try {
  const json = execFileSync("plutil", ["-convert", "json", "-o", "-", infoPlist], {
    encoding: "utf8",
  });
  plist = JSON.parse(json);
} catch (e) {
  fail(`could not read/parse Info.plist (${infoPlist}): ${e.message}`);
}
if (plist) {
  // CFBundleDocumentTypes ← tauri fileAssociations (wav/flac/mp3/… editors)
  const docTypes = plist.CFBundleDocumentTypes;
  if (Array.isArray(docTypes) && docTypes.length > 0) {
    const exts = new Set(docTypes.flatMap((d) => d.CFBundleTypeExtensions || []));
    const wanted = ["wav", "mp3", "flac", "m4a", "mp4", "mov"];
    const missing = wanted.filter((e) => !exts.has(e));
    if (missing.length === 0)
      pass(`CFBundleDocumentTypes: ${docTypes.length} types incl. ${wanted.join(", ")}`);
    else fail(`CFBundleDocumentTypes present but missing extensions: ${missing.join(", ")}`);
  } else {
    fail("CFBundleDocumentTypes missing/empty — fileAssociations did not reach Info.plist");
  }

  // LSMinimumSystemVersion ← bundle.macOS.minimumSystemVersion
  if (plist.LSMinimumSystemVersion) pass(`LSMinimumSystemVersion = ${plist.LSMinimumSystemVersion}`);
  else fail("LSMinimumSystemVersion missing — minimumSystemVersion did not apply");

  // NSMicrophoneUsageDescription ← Info.macos.plist merge (Recording Guard mic TCC prompt)
  if (plist.NSMicrophoneUsageDescription && plist.NSMicrophoneUsageDescription.length > 10)
    pass("NSMicrophoneUsageDescription present (Recording Guard mic prompt)");
  else fail("NSMicrophoneUsageDescription missing — mic capture will be killed by TCC");
}

// --- 5. code-signing integrity per Mach-O (untampered-since-signing) ---------------------------
// `codesign --verify --strict` proves a SIGNED Mach-O has not been altered since it was signed
// (its sealed hashes still match the bytes). That is a HARD failure when it fails — a signed but
// tampered sidecar must never ship. An UNSIGNED Mach-O is only informational here: this gate runs
// both BEFORE signing (Tauri's ad-hoc bundle; sherpa's upstream x86_64 slice is unsigned) and
// AFTER (sign-mac.mjs re-invokes it), and pre-signing an unsigned slice is expected.
//
// NOTE ON THE PIN GATE. This does NOT re-derive the anvil-media/anvil-asr *content-hash* pins
// (the signing-INDEPENDENT identity the engine enforces at run time). Re-implementing the Mach-O
// content hash in Node would be a second source of truth that could silently drift from the Rust
// one. Instead, the authoritative bundle-vs-pin proof is the mac-gated Rust integration test
// (`cargo test -p anvil-media --test sidecar_pin signed_bundle_ffmpeg_content_hash_matches_the_pin`
// and `cargo test -p anvil-asr --test bundled_layout signed_bundle_content_hash_matches_the_pins`),
// which runs against the built `.app` with the exact code the gate uses. See ADR-012.
section("[5] per-Mach-O signing integrity (codesign --verify --strict; content-hash pin gate lives in cargo test)");
const machO = [
  // the main executable: CFBundleExecutable is authoritative (still `anvil-desktop` — the Cargo
  // package name — post-rename); the no-dot scan covers a plist parse failure.
  path.join(macosDir, plist?.CFBundleExecutable || readdirSync(macosDir).find((f) => !f.includes(".")) || productName),
  ...binaries.map((b) => path.join(resources, b.rel)),
  path.join(resources, "sherpa/lib/libonnxruntime.1.17.1.dylib"),
];
for (const m of machO) {
  if (!existsSync(m)) continue;
  const rel = path.relative(appPath, m);
  const dv = spawnSync("codesign", ["-dv", m], { encoding: "utf8" });
  const info = `${dv.stdout || ""}${dv.stderr || ""}`;
  const isSigned = /Signature=/.test(info) || /flags=/.test(info);
  const flags = (info.match(/flags=\S+/) || [])[0] || "";
  const sig = (info.match(/Signature=\S+/) || [])[0] || "";
  if (!isSigned) {
    console.log(`  info  ${rel} — (no signature yet; pre-signing bundle)`);
    continue;
  }
  const v = spawnSync("codesign", ["--verify", "--strict", m], { encoding: "utf8" });
  const vOut = `${v.stdout || ""}${v.stderr || ""}`;
  const isAdhoc = /Signature=adhoc/.test(info) || /flags=\S*adhoc/.test(info);
  if (v.status === 0) pass(`${rel} — signed & untampered (${flags} ${sig})`.trim());
  else if (/not signed at all/i.test(vOut))
    // A fat binary with an unsigned slice (upstream sherpa universal2 ships its x86_64 slice
    // unsigned): codesign verifies ALL architectures by default, so this is the expected
    // PRE-signing state, not tamper. sign-mac.mjs signs every slice, so post-signing this
    // branch is unreachable and any real failure lands in the hard branch below.
    console.log(`  info  ${rel} — partially signed (an unsigned slice; pre-signing bundle)`);
  else if (isAdhoc)
    // Ad-hoc signatures are transient pre-signing artifacts. On arm64 the linker auto-signs the
    // main exe (flags=adhoc,linker-signed) with a seal that demands bundle resources Tauri has
    // not laid down ("code has no resources but signature indicates they must be present"), so
    // verify fails — expected, not tamper. sign-mac.mjs replaces every ad-hoc signature
    // wholesale; the untampered-since-signing gate keeps its teeth where it matters, because a
    // REAL (Developer ID) signature that fails verify still lands in the hard branch below.
    console.log(
      `  info  ${rel} — ad-hoc pre-signing state (${vOut.trim().split("\n")[0].split(": ").pop()})`,
    );
  else
    fail(
      `${rel} is SIGNED but codesign --verify --strict FAILED — tampered since signing:\n      ${vOut.trim().split("\n").slice(0, 3).join("\n      ")}`,
    );
}

// --- verdict -----------------------------------------------------------------------------------
console.log("");
if (failures.length > 0) {
  console.error(`verify-mac-bundle: ${failures.length} FAILURE(S) in ${path.basename(appPath)}`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`verify-mac-bundle: OK — ${path.basename(appPath)} is complete, executable, and spawnable env-free.`);
