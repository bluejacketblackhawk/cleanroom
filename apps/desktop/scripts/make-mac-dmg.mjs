#!/usr/bin/env node
// make-mac-dmg.mjs — build a distributable `.dmg` from the bundled `<productName>.app` (per
// tauri.conf.json, e.g. `Cleanroom.app`) using ONLY `hdiutil`, with no dependency on an
// interactive Finder session.
//
// WHY this exists. Tauri's own DMG bundler (`bundle_dmg.sh`) drives Finder over AppleScript
// (`osascript`) to set the drag-install window's background, icon positions, and size. That step
// REQUIRES a logged-in, automatable Finder — in a headless build, a CI runner, or an automation
// context without Apple-events permission it fails with "error running bundle_dmg.sh" AFTER the app
// bundle is already built. The `.app` is the real payload; the window styling is cosmetic. This
// script produces the functional artifact the same way every non-styled DMG is made: stage the
// `.app` next to an `/Applications` symlink and `hdiutil create` a compressed (UDZO) image. The
// result mounts, shows the .app + an Applications shortcut, and drag-installs exactly like a styled
// one — it just lacks the custom background. When run interactively, `npm run package:mac:*` uses
// Tauri's styled bundler; this is the headless/CI fallback and can be called directly.
//
// CLI CONTRACT (stable — the release lane may invoke it):
//   node apps/desktop/scripts/make-mac-dmg.mjs <path-to-.app>
//   node apps/desktop/scripts/make-mac-dmg.mjs --target <triple>   # locate the built .app
// Writes target/<triple|>/release/bundle/dmg/<productName>_<version>_<arch>.dmg (Tauri's
// path/naming) and prints its path, size, and sha256. Exit 0 on success, 1 on failure, 2 on
// usage error.

import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import path from "node:path";

const desktopDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const workspaceRoot = path.join(desktopDir, "..", "..");
const conf = JSON.parse(
  readFileSync(path.join(desktopDir, "src-tauri", "tauri.conf.json"), "utf8"),
);
const version = conf.version ?? "0.0.0";
const productName = conf.productName ?? "Cleanroom";

const args = process.argv.slice(2);
let triple = null;
const eq = args.find((a) => a.startsWith("--target="));
if (eq) triple = eq.slice("--target=".length);
const flagIdx = args.indexOf("--target");
if (flagIdx !== -1 && args[flagIdx + 1]) triple = args[flagIdx + 1];
const explicitApp = args.find((a) => a.endsWith(".app"));

const bundleDir = triple
  ? path.join(workspaceRoot, "target", triple, "release", "bundle")
  : path.join(workspaceRoot, "target", "release", "bundle");
// Deterministic bundle selection: exactly `<productName>.app` — never "the first .app in the
// dir", which once picked a stale pre-rename Cleanroom.app over Cleanroom.app and imaged the wrong
// bundle into a correctly-named DMG. Strays next to the real one are noted; a dir holding ONLY
// foreign .apps is a hard error. An explicit <path-to-.app> argument still wins unchanged.
const appPath = explicitApp
  ? path.resolve(explicitApp)
  : (() => {
      const macos = path.join(bundleDir, "macos");
      if (!existsSync(macos)) return null;
      const apps = readdirSync(macos).filter((f) => f.endsWith(".app"));
      const wanted = `${productName}.app`;
      if (apps.includes(wanted)) {
        const strays = apps.filter((a) => a !== wanted);
        if (strays.length)
          console.error(`make-mac-dmg: note — ignoring stale bundle(s) next to ${wanted}: ${strays.join(", ")}`);
        return path.join(macos, wanted);
      }
      if (apps.length) {
        console.error(
          `make-mac-dmg: ${wanted} (the configured productName) not found in ${macos}\n` +
            `  but the dir does contain: ${apps.join(", ")}\n` +
            `  Refusing to image an arbitrary .app — it could be a stale pre-rename bundle.\n` +
            `  Delete the stray bundle(s), or pass the intended .app path explicitly.`,
        );
        process.exit(2);
      }
      return null;
    })();

if (!appPath || !existsSync(appPath)) {
  console.error(
    "make-mac-dmg: could not find a .app.\n" +
      "  usage: node apps/desktop/scripts/make-mac-dmg.mjs <path-to-.app>\n" +
      "     or: node apps/desktop/scripts/make-mac-dmg.mjs --target <triple>",
  );
  process.exit(2);
}

// arch label for the filename, matching Tauri's macOS DMG naming (aarch64 stays, x86_64 → x64).
const arch = /x86_64/.test(triple || appPath) ? "x64" : "aarch64";
const dmgDir = path.join(bundleDir, "dmg");
const outDmg = path.join(dmgDir, `${productName}_${version}_${arch}.dmg`);

execFileSync("mkdir", ["-p", dmgDir]);
rmSync(outDmg, { force: true });

// Stage the .app + an Applications symlink in a scratch dir, then image that dir.
const stage = mkdtempSync(path.join(tmpdir(), "cleanroom-dmg-"));
try {
  // ditto preserves the bundle's Mach-O signatures + exec bits faithfully.
  execFileSync("ditto", [appPath, path.join(stage, path.basename(appPath))]);
  execFileSync("ln", ["-s", "/Applications", path.join(stage, "Applications")]);

  console.log(`Imaging ${path.basename(appPath)} → ${outDmg}`);
  execFileSync(
    "hdiutil",
    [
      "create",
      "-volname", productName,
      "-srcfolder", stage,
      "-fs", "HFS+",
      "-format", "UDZO", // zlib-compressed, read-only — the standard distributable DMG
      "-imagekey", "zlib-level=9",
      "-ov",
      outDmg,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
} catch (e) {
  console.error(`make-mac-dmg: hdiutil failed: ${e.message}`);
  rmSync(stage, { recursive: true, force: true });
  process.exit(1);
} finally {
  rmSync(stage, { recursive: true, force: true });
}

if (!existsSync(outDmg)) {
  console.error(`make-mac-dmg: hdiutil reported success but ${outDmg} is missing`);
  process.exit(1);
}
const size = statSync(outDmg).size;
const sha = execFileSync("shasum", ["-a", "256", outDmg], { encoding: "utf8" }).split(" ")[0];
console.log(`Wrote ${outDmg}`);
console.log(`  size:   ${(size / 1024 / 1024).toFixed(1)} MiB (${size} bytes)`);
console.log(`  sha256: ${sha}`);
