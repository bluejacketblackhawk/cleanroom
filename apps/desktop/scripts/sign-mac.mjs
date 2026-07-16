#!/usr/bin/env node
// sign-mac.mjs — Developer ID code-signing + notarization pipeline for the macOS `.app`/`.dmg`.
//
// WHY this exists (and why it is a POST-BUNDLE step, not Tauri config). Notarization has two hard
// rules that Tauri's own bundle-time signing cannot satisfy for THIS bundle:
//   1. Every bundled Mach-O must carry its OWN Developer ID signature with the hardened runtime and
//      a secure timestamp. Our three sidecars ship as `bundle.resources` — loose Mach-Os under
//      `Contents/Resources/` (ffmpeg, whisper-cli + 6 dylibs, sherpa exe + onnxruntime), NOT under
//      `Contents/Frameworks/`. `codesign <app>` (what Tauri runs when `signingIdentity` is set)
//      signs the main executable and SEALS those resources by hash, but does NOT give them their own
//      hardened-runtime Developer ID signature — so the app would notarize-reject on the first
//      sidecar. They must be signed INDIVIDUALLY, inside-out, before the `.app` is sealed.
//   2. Apple documents that `codesign --deep` is the WRONG tool for signing a distributable app
//      (it re-signs nested code with the OUTER options/identity and skips per-item entitlements).
//      So we sign each nested Mach-O explicitly, then the `.app` last WITH its entitlements. `--deep`
//      is used ONLY to VERIFY (that direction is fine and recommended).
// Hence ANVIL keeps Tauri UNSIGNED (no `bundle.macOS.signingIdentity` in tauri.macos.conf.json) and
// does ALL signing here, after the bundle exists. See docs/adr/012-mac-packaging.md.
//
// THE PIPELINE (flow matches ADR-012 §"Signed + notarized"):
//   build .app (npm run package:mac:{arm64,x64}) → [sign nested Mach-Os → sign .app] → verify →
//   make DMG (make-mac-dmg.mjs) → sign DMG → [notarize DMG → staple DMG → verify]
// The two [bracketed] halves: the FIRST (signing) runs today against the real Developer ID identity;
// the SECOND (notarize+staple) waits on a notarytool keychain profile the owner creates once
// (`xcrun notarytool store-credentials`). Everything here is a ready-to-run command either way.
//
// SIGNING RULES (honoring Apple's documented notarization requirements):
//   * NEVER `codesign --deep` to SIGN. Sign inside-out, each Mach-O individually, then the .app.
//   * Every signature: --force --options runtime (hardened runtime) --timestamp, Developer ID App.
//   * The .app is signed WITH entitlements.mac.plist (mic + disable-library-validation).
//     Sidecars get NO entitlements — plain hardened runtime. After re-signing the sherpa exe and its
//     sibling onnxruntime dylib with the SAME team identity, hardened-runtime library validation
//     accepts the (now same-team) dylib, so no disable-library-validation is needed on the sidecar.
//     `verify` proves this by spawning the signed sidecar env-free (verify-mac-bundle.mjs).
//   * Identity: --identity flag > $ANVIL_SIGN_IDENTITY > auto-detect the first "Developer ID
//     Application" from `security find-identity -v -p codesigning` (its SHA-1 hash, unambiguous).
//
// spctl INTERPRETATION (do not misread the signing-half output):
//   BEFORE notarization, `spctl --assess` MUST report *rejected* — source "Unnotarized Developer ID".
//   That is the EXPECTED SUCCESS STATE for the signing-only half: the code is validly Developer-ID
//   signed but Gatekeeper will not admit it until a notarization ticket exists. This script labels
//   that outcome explicitly so it is never mistaken for a failure. AFTER notarize+staple the same
//   assessment flips to *accepted* — source "Notarized Developer ID".
//
// CLI CONTRACT:
//   node apps/desktop/scripts/sign-mac.mjs <subcommand> [--target <triple|arch>|all] [options]
// subcommands:
//   sign         sign nested Mach-Os inside-out, then the .app with entitlements     (RUNS TODAY)
//   verify       codesign --verify --deep --strict + -dv + spctl + verify-mac-bundle (RUNS TODAY)
//   dmg          make the DMG from the SIGNED .app (make-mac-dmg.mjs) then sign the DMG (RUNS TODAY)
//   notarize     xcrun notarytool submit <dmg> --keychain-profile <profile> [--wait]  (needs profile)
//   staple       xcrun stapler staple <dmg> (+ best-effort the .app)                   (post-notarize)
//   release-mac  full chain: sign → verify → dmg → [notarize → staple → verify]
//                (the bracketed half runs only if --profile resolves; otherwise it stops after the
//                 signed DMG and prints the exact remaining owner command)
// options:
//   --target <triple|arch>   aarch64|arm64|x86_64|x64 | full *-apple-darwin triple | all  (default: all)
//   --identity <id>          signing identity override (else $ANVIL_SIGN_IDENTITY else auto-detect)
//   --profile <name>         notarytool keychain profile (default: anvil-notary)
//   --wait                   pass --wait to notarytool (block until the verdict)
//   --app <path> / --dmg <path>   operate on an explicit artifact (skips --target resolution)
// Exit 0 on success, 1 on failure, 2 on usage error.

import { execFileSync, spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  openSync,
  readdirSync,
  readFileSync,
  readSync,
  statSync,
} from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

// --- paths ------------------------------------------------------------------------------------
const scriptsDir = path.dirname(fileURLToPath(import.meta.url));
const desktopDir = path.dirname(scriptsDir);
const srcTauri = path.join(desktopDir, "src-tauri");
const workspaceRoot = path.join(desktopDir, "..", "..");
const entitlements = path.join(srcTauri, "entitlements.mac.plist");
const makeMacDmg = path.join(scriptsDir, "make-mac-dmg.mjs");
const verifyMacBundle = path.join(scriptsDir, "verify-mac-bundle.mjs");
const conf = JSON.parse(readFileSync(path.join(srcTauri, "tauri.conf.json"), "utf8"));
const version = conf.version ?? "0.0.0";
const productName = conf.productName ?? "ANVIL";

// codesign contacts Apple's timestamp server (and, on first key use, may raise a keychain
// authorization dialog). Bound every signing call so a blocked prompt is reported, never spun on.
const SIGN_TIMEOUT_MS = Number(process.env.ANVIL_SIGN_TIMEOUT_MS || 120000);

// --- tiny output helpers ----------------------------------------------------------------------
const C = { cyan: "\x1b[36m", green: "\x1b[32m", red: "\x1b[31m", yellow: "\x1b[33m", dim: "\x1b[2m", off: "\x1b[0m" };
const step = (m) => console.log(`\n${C.cyan}${m}${C.off}`);
const ok = (m) => console.log(`  ${C.green}ok${C.off}    ${m}`);
const info = (m) => console.log(`  ${C.dim}info${C.off}  ${m}`);
const warn = (m) => console.log(`  ${C.yellow}warn${C.off}  ${m}`);
const note = (m) => console.log(`  ${C.yellow}note${C.off}  ${m}`);
function die(msg, code = 1) {
  console.error(`${C.red}ERROR (sign-mac):${C.off} ${msg}`);
  process.exit(code);
}

// --- subprocess helpers -----------------------------------------------------------------------
// run: inherit stdio (user sees codesign/notarytool output), throw on nonzero, surface timeouts.
function run(cmd, args, { timeout } = {}) {
  const r = spawnSync(cmd, args, { stdio: "inherit", timeout });
  if (r.error && r.error.code === "ETIMEDOUT") {
    const e = new Error("ETIMEDOUT");
    e.timedOut = true;
    throw e;
  }
  if (r.error) throw r.error;
  if (r.status !== 0) {
    const e = new Error(`${cmd} exited ${r.status}`);
    e.status = r.status;
    throw e;
  }
}
// cap: capture combined output, never throws (caller inspects status).
function cap(cmd, args, { timeout } = {}) {
  const r = spawnSync(cmd, args, { encoding: "utf8", timeout });
  return { status: r.status, out: `${r.stdout || ""}${r.stderr || ""}`, error: r.error };
}

// --- arch / target normalization --------------------------------------------------------------
function normalizeTriple(sel) {
  if (!sel) return null;
  if (/x86_64|x64|amd64|intel/i.test(sel)) return "x86_64-apple-darwin";
  if (/aarch64|arm64|apple.?silicon/i.test(sel)) return "aarch64-apple-darwin";
  return null;
}
const archLabel = (triple) => (/x86_64/.test(triple) ? "x64" : "aarch64");

// --- artifact locators ------------------------------------------------------------------------
function findApp(triple, explicit) {
  if (explicit) return path.resolve(explicit);
  const macos = path.join(workspaceRoot, "target", triple, "release", "bundle", "macos");
  if (!existsSync(macos)) return null;
  const a = readdirSync(macos).find((f) => f.endsWith(".app"));
  return a ? path.join(macos, a) : null;
}
function findDmg(triple, explicit) {
  if (explicit) return path.resolve(explicit);
  const dmg = path.join(
    workspaceRoot, "target", triple, "release", "bundle", "dmg",
    `${productName}_${version}_${archLabel(triple)}.dmg`,
  );
  return existsSync(dmg) ? dmg : null;
}

// --- identity resolution ----------------------------------------------------------------------
let cachedIdentity = null;
function resolveIdentity(flag) {
  if (cachedIdentity) return cachedIdentity;
  const override = flag || process.env.ANVIL_SIGN_IDENTITY;
  if (override) {
    cachedIdentity = { id: override, name: override, source: flag ? "--identity" : "$ANVIL_SIGN_IDENTITY" };
    return cachedIdentity;
  }
  const r = cap("security", ["find-identity", "-v", "-p", "codesigning"]);
  if (r.status !== 0) die(`security find-identity failed:\n${r.out}`);
  const line = r.out.split("\n").find((l) => /Developer ID Application/.test(l));
  if (!line) {
    die(
      "no \"Developer ID Application\" identity in the login keychain.\n" +
        "  Install one from the Apple Developer portal, or pass --identity / $ANVIL_SIGN_IDENTITY.\n" +
        `  security find-identity said:\n${r.out}`,
    );
  }
  const hash = (line.match(/\b([0-9A-Fa-f]{40})\b/) || [])[1];
  const name = (line.match(/"([^"]+)"/) || [])[1] || "Developer ID Application";
  if (!hash) die(`could not parse an identity hash from: ${line.trim()}`);
  cachedIdentity = { id: hash, name, source: "auto-detect (security find-identity)" };
  return cachedIdentity;
}

// --- Mach-O discovery -------------------------------------------------------------------------
// A file is a Mach-O if its first 4 bytes are a thin (LE on disk) or fat/universal (BE on disk)
// Mach-O magic. We only probe files that are executable or end in .dylib, so models/wav/txt are
// never opened. This yields exactly the nested sidecar binaries, discovered — not hardcoded.
const MACHO_MAGIC = new Set([
  0xfeedface, 0xfeedfacf, // thin, big-endian magic
  0xcefaedfe, 0xcffaedfe, // thin, little-endian on disk (arm64/x86_64)
  0xcafebabe, 0xcafebabf, // fat / universal (big-endian on disk)
]);
function isMachO(file) {
  let fd;
  try {
    fd = openSync(file, "r");
    const buf = Buffer.alloc(4);
    if (readSync(fd, buf, 0, 4, 0) < 4) return false;
    return MACHO_MAGIC.has(buf.readUInt32BE(0));
  } catch {
    return false;
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
}
function walk(dir, out) {
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) walk(p, out);
    else if (st.isFile()) out.push({ path: p, mode: st.mode });
  }
}
// The nested Mach-Os to sign, ordered leaves-first: dylibs before the executables that load them.
function nestedMachOs(app) {
  const resources = path.join(app, "Contents", "Resources");
  const all = [];
  walk(resources, all);
  const machos = all
    .filter((f) => (f.mode & 0o111 || f.path.endsWith(".dylib")) && isMachO(f.path))
    .map((f) => f.path);
  machos.sort((a, b) => {
    const rank = (p) => (p.endsWith(".dylib") ? 0 : 1); // dylibs first
    return rank(a) - rank(b) || a.localeCompare(b);
  });
  return machos;
}

// --- signing ----------------------------------------------------------------------------------
function codesign(file, id, { entitlements: ents } = {}) {
  const args = ["--force", "--options", "runtime", "--timestamp", "--sign", id];
  if (ents) args.push("--entitlements", ents);
  args.push(file);
  try {
    run("codesign", args, { timeout: SIGN_TIMEOUT_MS });
  } catch (e) {
    if (e.timedOut) {
      console.error("");
      die(
        `codesign timed out after ${SIGN_TIMEOUT_MS / 1000}s on:\n    ${file}\n` +
          "  This almost always means a KEYCHAIN AUTHORIZATION dialog is waiting for a click and\n" +
          "  nobody is at the screen. On the Mac, a dialog titled roughly:\n" +
          `    \"codesign wants to sign using key \\\"${id}\\\" in your keychain\"\n` +
          "  is open. Click \"Always Allow\" (not just \"Allow\", so the remaining files don't re-prompt),\n" +
          "  then re-run this command. If instead the timestamp server is unreachable, check the network.",
        1,
      );
    }
    throw e;
  }
}

function signApp(triple, explicitApp, idFlag) {
  const app = findApp(triple, explicitApp);
  if (!app || !existsSync(app)) {
    die(
      `no .app found for ${triple}. Build it first:\n` +
        `    (cd apps/desktop && npm run package:mac:${triple.startsWith("x86_64") ? "x64" : "arm64"})`,
    );
  }
  const identity = resolveIdentity(idFlag);
  step(`sign — ${path.relative(workspaceRoot, app)}`);
  info(`identity: ${identity.name}  ${C.dim}[${identity.source}]${C.off}`);
  if (!existsSync(entitlements)) die(`entitlements file missing: ${entitlements}`);

  // 1) nested Mach-Os, inside-out, individually, NO entitlements (plain hardened runtime).
  const nested = nestedMachOs(app);
  const EXPECTED = [
    "ffmpeg/ffmpeg",
    "whisper/whisper-cli",
    "whisper/libwhisper.1.dylib", "whisper/libggml.0.dylib", "whisper/libggml-base.0.dylib",
    "whisper/libggml-cpu.0.dylib", "whisper/libggml-metal.0.dylib", "whisper/libggml-blas.0.dylib",
    "sherpa/bin/sherpa-onnx-offline-speaker-diarization",
    "sherpa/lib/libonnxruntime.1.17.1.dylib",
  ];
  const rel = (p) => path.relative(path.join(app, "Contents", "Resources"), p);
  const foundRel = new Set(nested.map(rel));
  const missing = EXPECTED.filter((e) => !foundRel.has(e));
  if (missing.length) warn(`expected sidecar Mach-O(s) not found (bundle may be incomplete): ${missing.join(", ")}`);
  step(`sign nested Mach-Os (inside-out, ${nested.length} found — hardened runtime, timestamp, no entitlements)`);
  for (const m of nested) {
    codesign(m, identity.id);
    ok(rel(m));
  }

  // 2) the .app LAST, WITH entitlements (mic + disable-library-validation).
  step("sign the .app bundle (last) — WITH entitlements.mac.plist");
  codesign(app, identity.id, { entitlements });
  ok(`${path.basename(app)} sealed with Developer ID + hardened runtime + entitlements`);
  return app;
}

// --- verification -----------------------------------------------------------------------------
function spctlAssess(target, extra, { label }) {
  const r = cap("spctl", ["--assess", "--verbose=4", ...extra, target]);
  const accepted = r.status === 0 && /accepted/i.test(r.out);
  const rejected = !accepted;
  const unnotarized = /Unnotarized Developer ID/i.test(r.out);
  const notarized = /Notarized Developer ID/i.test(r.out);
  const trimmed = r.out.trim().split("\n").map((l) => `        ${l}`).join("\n");
  console.log(`  ${C.dim}spctl${C.off} ${label}:\n${trimmed}`);
  if (accepted && notarized) ok(`spctl ACCEPTED — Notarized Developer ID (Gatekeeper will admit it)`);
  else if (rejected && unnotarized)
    note(
      `spctl REJECTED — \"Unnotarized Developer ID\". This is the EXPECTED, CORRECT state after the\n` +
        `        signing half and BEFORE notarization: the code is validly Developer-ID signed but has\n` +
        `        no notarization ticket yet. It flips to ACCEPTED after notarize + staple. NOT a failure.`,
    );
  else if (accepted) ok(`spctl ACCEPTED`);
  else warn(`spctl rejected for an unexpected reason (see output above)`);
  return { accepted, rejected, unnotarized, notarized };
}

function verifyApp(triple, explicitApp) {
  const app = findApp(triple, explicitApp);
  if (!app || !existsSync(app)) die(`no .app found for ${triple} to verify`);
  step(`verify — ${path.relative(workspaceRoot, app)}`);

  // 1) deep + strict VERIFY (this direction of --deep is fine and recommended).
  const v = cap("codesign", ["--verify", "--deep", "--strict", "--verbose=2", app]);
  if (v.status === 0) ok("codesign --verify --deep --strict passed");
  else die(`codesign --verify --deep --strict FAILED:\n${v.out}`);

  // 2) -dv detail: confirm Authority = Developer ID Application + hardened runtime flag.
  const dv = cap("codesign", ["-dv", "--verbose=4", app]);
  const authority = (dv.out.match(/Authority=Developer ID Application[^\n]*/) || [])[0] || "(authority not found)";
  const team = (dv.out.match(/TeamIdentifier=\S+/) || [])[0] || "";
  const runtime = /flags=\S*runtime/i.test(dv.out) || / runtime(\b|$)/i.test(dv.out) ? "runtime ON" : "runtime NOT set";
  const timestamp = (dv.out.match(/Timestamp=[^\n]*/) || [])[0] || "(no secure timestamp)";
  info(authority);
  if (team) info(team);
  info(`hardened runtime: ${runtime}`);
  info(timestamp);
  if (!/flags=\S*runtime/i.test(dv.out)) warn("hardened runtime flag not detected on the .app — check signing options");

  // 3) per-sidecar signature proof (each nested Mach-O individually Developer-ID + runtime signed).
  step("per-sidecar signature check (each nested Mach-O)");
  for (const m of nestedMachOs(app)) {
    const relp = path.relative(path.join(app, "Contents", "Resources"), m);
    const sv = cap("codesign", ["--verify", "--strict", m]);
    const sd = cap("codesign", ["-dv", "--verbose=2", m]);
    const devid = /Authority=Developer ID Application/.test(sd.out);
    const rt = /flags=\S*runtime/i.test(sd.out);
    if (sv.status === 0 && devid && rt) ok(`${relp} — Developer ID + runtime`);
    else warn(`${relp} — verify=${sv.status === 0 ? "ok" : "FAIL"} devid=${devid} runtime=${rt}`);
  }

  // 4) Gatekeeper assessment (pre-notarization: expect the labeled rejection).
  step("Gatekeeper assessment (spctl)");
  const gk = spctlAssess(app, ["--type", "execute"], { label: "--type execute (.app)" });

  // 5) re-run the bundle completeness/exec-bit/env-free-spawn gate on the SIGNED app.
  step("verify-mac-bundle.mjs on the signed .app (sidecars must still spawn env-free)");
  const vb = cap(process.execPath, [verifyMacBundle, app]);
  process.stdout.write(vb.out.endsWith("\n") ? vb.out : vb.out + "\n");
  if (vb.status !== 0) die("verify-mac-bundle.mjs FAILED on the signed .app (see output above)");
  ok("verify-mac-bundle.mjs passed on the signed .app");

  // 6) content-hash pin gate (the identity anvil-media/anvil-asr enforce at run time). Re-signing
  //    rewrites each sidecar's raw bytes, so the runtime gate pins the signing-INDEPENDENT Mach-O
  //    *content* hash. That bundle-vs-pin proof is a mac-gated Rust integration test (run against
  //    this very `.app`), not re-implemented here — a second Mach-O parser in Node could drift from
  //    the engine's. Surface the exact command so the release operator runs it after signing.
  const appTriple = triple || (/x86_64/.test(app) ? "x86_64-apple-darwin" : "aarch64-apple-darwin");
  note(
    "content-hash pin gate (signing-independent identity) is verified by cargo test, not this\n" +
      "        script (single source of truth — see ADR-012 §content-hash pin). Run against the signed .app:\n" +
      `        cargo test -p anvil-media --test sidecar_pin signed_bundle_ffmpeg_content_hash_matches_the_pin\n` +
      `        cargo test -p anvil-asr   --test bundled_layout signed_bundle_content_hash_matches_the_pins\n` +
      `        # (the built .app for ${appTriple} must be present, which it is once 'sign' has run)`,
  );
  return { app, gatekeeper: gk };
}

// --- DMG (make from the SIGNED app, then sign the DMG) -----------------------------------------
function makeAndSignDmg(triple, idFlag) {
  step(`dmg — build from the signed .app, then sign the DMG (${triple})`);
  const app = findApp(triple);
  if (!app) die(`no .app for ${triple}; run 'sign' first`);
  // make-mac-dmg.mjs uses `ditto`, preserving the freshly-applied signatures + exec bits.
  run(process.execPath, [makeMacDmg, "--target", triple]);
  const dmg = findDmg(triple);
  if (!dmg || !existsSync(dmg)) die(`make-mac-dmg produced no DMG for ${triple}`);
  const identity = resolveIdentity(idFlag);
  // A disk image is signed (Developer ID + secure timestamp) but hosts no runtime, so NO
  // --options runtime and NO entitlements — just the outer signature notarization will bless.
  step(`sign the DMG — ${path.basename(dmg)}`);
  try {
    run("codesign", ["--force", "--timestamp", "--sign", identity.id, dmg], { timeout: SIGN_TIMEOUT_MS });
  } catch (e) {
    if (e.timedOut) die(`codesign timed out signing the DMG (keychain prompt?): ${dmg}`);
    throw e;
  }
  ok(`DMG signed with Developer ID + timestamp`);
  const sha = execFileSync("shasum", ["-a", "256", dmg], { encoding: "utf8" }).split(" ")[0];
  const size = statSync(dmg).size;
  info(`path:   ${dmg}`);
  info(`size:   ${(size / 1024 / 1024).toFixed(1)} MiB (${size} bytes)`);
  info(`sha256: ${sha}`);
  // DMG Gatekeeper assessment (pre-notarization: expect the labeled rejection).
  spctlAssess(dmg, ["--type", "open", "--context", "context:primary-signature"], { label: "--type open (.dmg)" });
  return { dmg, sha, size };
}

// --- notarize / staple ------------------------------------------------------------------------
function profileExists(profile) {
  // notarytool stores its profile as a keychain generic-password item; a cheap history probe tells
  // us whether the profile resolves without actually submitting anything.
  const r = cap("xcrun", ["notarytool", "history", "--keychain-profile", profile]);
  if (r.status === 0) return true;
  if (/No Keychain (item|password item) found for profile/i.test(r.out)) return false;
  // Any other error (e.g. transient network) — treat as "present but errored" so we still attempt.
  return true;
}

function notarize(triple, explicitDmg, profile, wait) {
  const dmg = findDmg(triple, explicitDmg);
  if (!dmg || !existsSync(dmg)) die(`no signed DMG for ${triple}; run 'dmg' first`);
  step(`notarize — ${path.basename(dmg)}  (profile: ${profile})`);
  if (!profileExists(profile)) {
    die(
      `notarytool keychain profile \"${profile}\" does not exist yet.\n` +
        "  This is the ONE remaining owner step. Create it once, then re-run:\n" +
        `    xcrun notarytool store-credentials ${profile} --apple-id <id> --team-id 7Y39A984XL --password <app-specific-pw>\n` +
        `    node apps/desktop/scripts/sign-mac.mjs notarize --profile ${profile} --wait --target ${archLabel(triple) === "x64" ? "x86_64" : "aarch64"}`,
      1,
    );
  }
  const args = ["notarytool", "submit", dmg, "--keychain-profile", profile];
  if (wait) args.push("--wait");
  info(`xcrun ${args.join(" ")}`);
  run("xcrun", args); // no timeout — --wait can legitimately take minutes on Apple's side
  ok("notarytool submission complete" + (wait ? " (verdict above)" : " (poll with 'xcrun notarytool info')"));
  return dmg;
}

function staple(triple, explicitDmg) {
  const dmg = findDmg(triple, explicitDmg);
  if (!dmg || !existsSync(dmg)) die(`no DMG for ${triple} to staple`);
  step(`staple — ${path.basename(dmg)}`);
  run("xcrun", ["stapler", "staple", dmg]);
  ok("stapled the DMG (offline Gatekeeper ticket attached)");
  // Best-effort: staple the .app too (same cdhash was notarized inside the DMG), so an app dragged
  // out of the image validates offline as well. Non-fatal if the ticket hasn't propagated yet.
  const app = findApp(triple);
  if (app) {
    const r = cap("xcrun", ["stapler", "staple", app]);
    if (r.status === 0) ok(`also stapled ${path.basename(app)}`);
    else warn(`could not staple the .app (non-fatal; DMG is stapled): ${r.out.trim().split("\n")[0]}`);
  }
  step("post-staple Gatekeeper assessment");
  spctlAssess(dmg, ["--type", "open", "--context", "context:primary-signature"], { label: "--type open (.dmg)" });
  if (app) spctlAssess(app, ["--type", "execute"], { label: "--type execute (.app)" });
  return dmg;
}

// --- orchestration ----------------------------------------------------------------------------
function releaseMac(triple, opts) {
  signApp(triple, opts.app, opts.identity);
  verifyApp(triple, opts.app);
  const { dmg, sha } = makeAndSignDmg(triple, opts.identity);
  const profile = opts.profile || "anvil-notary";
  if (profileExists(profile)) {
    notarize(triple, dmg, profile, true);
    staple(triple, dmg);
    verifyApp(triple);
    step(`release-mac COMPLETE for ${triple}`);
    ok(`signed + notarized + stapled: ${dmg}`);
    ok(`sha256: ${sha}`);
  } else {
    step(`release-mac: signing half DONE for ${triple} — notarization WAITS on profile \"${profile}\"`);
    ok(`signed .app + signed DMG ready: ${dmg}`);
    ok(`sha256: ${sha}`);
    note(
      "spctl currently REJECTS (\"Unnotarized Developer ID\") — expected until notarization. To finish:\n" +
        `        xcrun notarytool store-credentials ${profile} --apple-id <id> --team-id 7Y39A984XL --password <app-specific-pw>\n` +
        `        node apps/desktop/scripts/sign-mac.mjs release-mac --target ${archLabel(triple) === "x64" ? "x86_64" : "aarch64"} --profile ${profile}\n` +
        `        # (or just the notarize+staple tail: sign-mac.mjs notarize --wait && sign-mac.mjs staple, same --target)`,
    );
  }
}

// --- CLI --------------------------------------------------------------------------------------
function parseArgs(argv) {
  const a = argv.slice(2);
  const sub = a[0];
  const opts = { target: "all", profile: "anvil-notary", wait: false, identity: null, app: null, dmg: null };
  for (let i = 1; i < a.length; i++) {
    const t = a[i];
    const val = (inline) => (inline !== undefined ? inline : a[++i]);
    if (t === "--wait") opts.wait = true;
    else if (t.startsWith("--target=")) opts.target = t.slice(9);
    else if (t === "--target") opts.target = a[++i];
    else if (t.startsWith("--profile=")) opts.profile = t.slice(10);
    else if (t === "--profile") opts.profile = a[++i];
    else if (t.startsWith("--identity=")) opts.identity = t.slice(11);
    else if (t === "--identity") opts.identity = a[++i];
    else if (t.startsWith("--app=")) opts.app = t.slice(6);
    else if (t === "--app") opts.app = a[++i];
    else if (t.startsWith("--dmg=")) opts.dmg = t.slice(6);
    else if (t === "--dmg") opts.dmg = a[++i];
    else if (t.startsWith("--")) void val();
  }
  return { sub, opts };
}

function targetsFor(opts, explicit) {
  if (explicit) return [null]; // an explicit --app/--dmg pins the artifact; triple is unused
  if (opts.target === "all") return ["aarch64-apple-darwin", "x86_64-apple-darwin"];
  const t = normalizeTriple(opts.target);
  if (!t) die(`unrecognized --target \"${opts.target}\" (use aarch64 | x86_64 | a *-apple-darwin triple | all)`, 2);
  return [t];
}

function usage(code) {
  console.log(
    "sign-mac.mjs — Developer ID signing + notarization for the macOS .app/.dmg\n\n" +
      "  node apps/desktop/scripts/sign-mac.mjs <sign|verify|dmg|notarize|staple|release-mac> \\\n" +
      "       [--target aarch64|x86_64|all] [--identity <id>] [--profile anvil-notary] [--wait]\n\n" +
      "  sign         sign nested Mach-Os inside-out then the .app (entitlements) — RUNS TODAY\n" +
      "  verify       codesign --verify --deep --strict + -dv + spctl + verify-mac-bundle — RUNS TODAY\n" +
      "  dmg          make the DMG from the signed .app then sign the DMG — RUNS TODAY\n" +
      "  notarize     xcrun notarytool submit --keychain-profile <profile> [--wait] — needs the profile\n" +
      "  staple       xcrun stapler staple the DMG (+ the .app) — after notarization\n" +
      "  release-mac  sign → verify → dmg → [notarize → staple → verify] (bracket runs iff --profile resolves)\n",
  );
  process.exit(code);
}

function main() {
  const { sub, opts } = parseArgs(process.argv);
  if (!sub || sub === "-h" || sub === "--help" || sub === "help") usage(sub ? 0 : 2);

  const explicitApp = opts.app;
  const explicitDmg = opts.dmg;

  try {
    switch (sub) {
      case "sign":
        for (const t of targetsFor(opts, explicitApp)) signApp(t, explicitApp, opts.identity);
        break;
      case "verify":
        for (const t of targetsFor(opts, explicitApp)) verifyApp(t, explicitApp);
        break;
      case "dmg":
        for (const t of targetsFor(opts, explicitDmg)) makeAndSignDmg(t, opts.identity);
        break;
      case "notarize":
        for (const t of targetsFor(opts, explicitDmg)) notarize(t, explicitDmg, opts.profile, opts.wait);
        break;
      case "staple":
        for (const t of targetsFor(opts, explicitDmg)) staple(t, explicitDmg);
        break;
      case "release-mac":
        for (const t of targetsFor(opts, explicitApp)) releaseMac(t, opts);
        break;
      default:
        die(`unknown subcommand \"${sub}\"`, 2);
    }
  } catch (e) {
    die(e.message || String(e));
  }
  console.log("");
  ok(`${sub} done`);
}

main();
