# ADR-011: Windows Packaging, Updater & Uninstall Hygiene

- Status: Accepted (installer/updater config), signing method Deferred (owner decision)
- Date: 2026-07-14
- Source: handoff/05-MILESTONES.md § M5 lane A/B, handoff/06-QUALITY-EVAL.md § 6, handoff/07-RISKS-LEGAL.md § 3

## Context

M5 ships the first real distributable of Cleanroom. It needs an installer a non-technical
podcaster can run without admin rights, a way to opt out of installing at all (portable),
a real update path once a repo exists to update from, and — because `anvil-core::platform`
(ADR-006) already writes per-user registry keys for the Explorer context menu, file
associations, and autostart — an uninstall path that actually removes them.

## Decision

**Installer:** Tauri's NSIS bundler (`apps/desktop/src-tauri/tauri.conf.json`,
`bundle.windows.nsis`), `installMode: "currentUser"` (no admin, installs under
`%LOCALAPPDATA%`), one target (`bundle.targets: ["nsis"]` — no MSI; nothing in the spec
asks for it and it doubles CI bundle time for a second installer format nobody requested).
`webviewInstallMode: "embedBootstrapper"` — the small (~2 MB) WebView2 bootstrapper ships
inside the installer so a fresh machine doesn't need a separate network hop mid-install;
Windows 11 and most updated Windows 10 machines already have the Evergreen runtime, so this
only matters on the stragglers.

**Portable:** Tauri has no first-party "portable" bundle target — a Tauri app is already
one self-contained exe (no bundled sidecars; ffmpeg is fetched at first use by
`anvil-media`'s sidecar manager, same on both distributions), so "portable" is just that
exe zipped with a README and no installer/registry writes. `apps/desktop/scripts/
package-portable.mjs` builds it via PowerShell's `Compress-Archive` after `tauri build`
(or a bare `cargo build --release -p anvil-desktop`) has produced the exe — no new zip
library dependency for a script that only ever runs on the platform it packages for.

**Uninstall hygiene:** the NSIS uninstaller doesn't know about
`anvil-core::platform`'s runtime-written HKCU keys (context menu, "Open with" list,
autostart) — the installer never wrote them; Settings' Integration toggles did, if the
user ever turned them on. `apps/desktop/src-tauri/installer-hooks.nsh` hooks
`NSIS_HOOK_PREUNINSTALL` (runs while `$INSTDIR` and the exe still exist, before any
deletion) to `ExecWait` `<exe> --uninstall-cleanup`, a bare CLI flag (`main.rs`) that runs
the same idempotent unregister calls unconditionally. Idempotent means this is safe to run
even when the user never enabled anything.

**Updater:** `tauri-plugin-updater`, wired for real (Rust plugin registered in `lib.rs`,
`@tauri-apps/plugin-updater` used from Settings' "Check for updates" button — no automatic
check-on-launch, so a not-yet-configured feed never surprises a user with an error on
startup). `tauri.conf.json`'s `plugins.updater.endpoints`/`pubkey` are explicit TODO
placeholders — **the repo does not exist yet** (owner ships everything at once, per
project convention), so there is no real org/repo to point at and no signing keypair has
been generated. `createUpdaterArtifacts` stays `false` until a real signing key exists;
turning it on without one fails the build outright, which is the correct failure mode
(loud, not a silently-broken update feed).

**Code signing (Authenticode):** deferred to the owner, per 07 §3 — SignPath.io's free OSS
program vs Azure Trusted Signing (~$10/mo), "decide by M5.B." `.github/workflows/
release.yml` has both signing steps wired and correctly parameterized, gated on a
`SIGNING_METHOD` repo variable that isn't set — so today's builds are honestly unsigned
(SmartScreen will warn) rather than fake-signed or blocked on a decision nobody's made yet.

## Consequences

**Enables:** a working, testable install/uninstall/portable path today without waiting on
the org/repo or a signing decision; the update feed and Authenticode signing slot in later
by filling in config, not by re-architecting anything.

**Constrains:**
- Anyone adding a new HKCU-writing capability to `anvil-core::platform` must also add its
  unregister call to `uninstall_cleanup()` (`apps/desktop/src-tauri/src/lib.rs`) or
  uninstall hygiene silently regresses.
- The updater's "Check for updates" button will show a clear "not configured yet" error
  until the owner: (1) creates the real GitHub org/repo and updates
  `plugins.updater.endpoints`, (2) runs `npm run tauri signer generate`, pastes the public
  key into `plugins.updater.pubkey`, and stores the private key + password as
  `TAURI_SIGNING_PRIVATE_KEY(_PASSWORD)` GitHub Actions secrets, (3) flips
  `bundle.createUpdaterArtifacts` to `true`.
- Release binaries stay unsigned (SmartScreen-flagged) until the owner sets the
  `SIGNING_METHOD` repo variable and the matching secrets from `.github/workflows/
  release.yml`'s signing steps.
