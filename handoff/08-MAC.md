# 08 — macOS port (M6) kickoff

> **Historical (2026-07-15):** M6 executed; see `STATE.md` §M6 and `docs/adr/012-mac-packaging.md`
> for what actually shipped. Three assumptions below were corrected by measurement: the exec bit
> DOES survive Tauri 2.9.3's resource copy (§4), `ort` on mac-arm64 is statically linked (no
> dylib to stage — the DirectML.dll analog applies only to Intel via `load-dynamic`, §2), and
> the ship shape is per-arch artifacts, not universal2 (owner's 6-artifact directive, ADR-012).

The Windows build is functionally complete and shipping-ready (installer builds, all engines wired,
sidecars bundled, launches clean). This file is the fast-start for the macOS pass so you don't
re-derive what Windows already settled. Read `STATE.md` for status and `05-MILESTONES.md` §M6 for the
milestone framing; this is the concrete "how".

## What already works (do NOT redo)
- **All 12 crates + the app are cross-platform Rust.** The engine (DSP chain, streaming master,
  diarization, cut, multitrack, clip, batch) is OS-agnostic. It compiles and unit-tests on macOS
  today; only `anvil-core::platform` has OS `#[cfg]` (Windows registry/shell integration — the one
  place needing a macOS sibling).
- **The provisioning pattern is established** — mirror it, don't reinvent: `scripts/fetch-*.ps1` +
  `scripts/*-pin.json` (immutable release URL + sha256 + license gate) → gitignored `vendor/` →
  `tauri.conf.json` `bundle.resources` places each next to the app → each crate's `locate()` resolves
  it. Windows pins: ffmpeg (BtbN LGPL 8.1), whisper.cpp v1.9.1, sherpa-onnx v1.12.14.
- **Cross-platform assets — REUSE, don't re-pin:** the diarization ONNX models
  (`pyannote-segmentation-3.0`, MIT; `NeMo TitaNet-small`, **CC-BY-4.0 — the About screen already
  credits it, keep that**) and the whisper ggml weights are platform-independent. Only the *binaries*
  are per-OS.

## The M6 job, narrowed
Everything platform-specific reduces to five things:
1. macOS builds of three sidecar **binaries** (ffmpeg, whisper.cpp, sherpa-onnx) + their dylibs.
2. `ort`/onnxruntime: DirectML doesn't exist on macOS — swap to CoreML/CPU.
3. The `.app` bundle layout vs `locate()`'s search path.
4. macOS bundle (`.dmg`/`.app`), universal (arm64 + x86_64), the +x/exec bit on sidecars.
5. Code-signing + notarization (gated on an Apple Developer account — owner decision).

Do them roughly in that order. 1–3 get a working dev build; 4–5 make it shippable.

## 1. macOS sidecars (the real work)
Target **both** arch: `aarch64-apple-darwin` (Apple Silicon) and `x86_64-apple-darwin` (Intel), or a
universal2 binary. Mirror `fetch-*.ps1` as `fetch-*.sh` (or make them cross-platform) with the same
pin+sha256+license discipline, staging to `vendor/<name>/darwin-<arch>/`.

- **ffmpeg (LGPL) — hardest item.** Most prebuilt macOS ffmpegs (evermeet, Homebrew) are **GPL** —
  do not ship those. Safest path: **build from source**, `./configure` WITHOUT `--enable-gpl` /
  `--enable-nonfree`, no libx264/libx265/libfdk, and keep the same GPL-marker gate the Windows
  `fetch-ffmpeg.ps1` runs on the configure line. Enable `videotoolbox` (a macOS system framework) —
  the Clip Studio H.264 path already allowlists `h264_videotoolbox` in `anvil-media/src/clip.rs`, so
  video export works once ffmpeg exposes it. Pin the built artifact by sha256 like the Windows one.
- **whisper.cpp (MIT).** Their GitHub releases are Windows-focused; on macOS you'll likely `make`
  from the `v1.9.1` tag (Metal is on by default — free GPU accel, no special hardware needed, which
  is on-brand). Produces `whisper-cli` + `libwhisper.dylib` + `libggml*.dylib`. Bundle the binary +
  all its dylibs (same lesson as the Windows DLL set). Ship the plain build; keep it MIT-only.
- **sherpa-onnx (Apache-2.0).** They DO publish macOS releases
  (`sherpa-onnx-vX.Y.Z-osx-universal2-shared.tar.bz2`). Take the diarization binary +
  `libonnxruntime*.dylib`. Reuse the same `.onnx` models Windows uses.

## 2. ort: DirectML → CoreML/CPU
`anvil-ai` drives DeepFilterNet3 through `ort`. On Windows it uses the **DirectML** execution
provider (which is why `DirectML.dll` had to be bundled — see the Windows saga below). macOS has no
DirectML. In `crates/anvil-ai/Cargo.toml`, gate the ort feature per target: keep `directml` on
`cfg(windows)`, use **`coreml`** (or plain CPU) on `cfg(target_os = "macos")`. CoreML is a system
framework (always present), so there is **no DirectML.dll-style DLL to bundle** — but `ort` will
still emit a `libonnxruntime*.dylib` next to the target that the app loads. **Bundle that dylib next
to the app exe** (the exact trap that bit Windows — see below). Re-verify the DNSMOS gate after the
EP swap (BAK ≥ +1.0); CoreML vs DirectML can differ numerically.

## 3. `.app` layout vs locate()
This is the key packaging gotcha. `locate()` today searches `env → <exe_dir>/<name>/ → PATH`, and
the models dir is `<exe_dir>/models`. In a macOS `.app`, the binary lives at
`Anvil.app/Contents/MacOS/anvil`, but Tauri puts `bundle.resources` in
`Anvil.app/Contents/Resources/`. So `<exe_dir>/ffmpeg/` (i.e. `Contents/MacOS/ffmpeg/`) will be
empty. Fix ONE of:
- teach `locate()` (and `anvil-asr::models_dirs`) to also check `../Resources/<name>/` on macOS, or
- configure the bundle to place sidecars under `Contents/MacOS/` alongside the binary.
Prefer the `locate()` fix — it's small and keeps the resource story clean. Add a macOS `bundled_layout`
test that mirrors the Windows re-exec test.

## 4. macOS bundle
- Tauri emits `.app` + `.dmg`. Build universal: `tauri build --target universal-apple-darwin` (needs
  both rust targets installed).
- **Exec bit:** Tauri `bundle.resources` does NOT preserve the Unix +x bit, so bundled `ffmpeg` /
  `whisper-cli` / sherpa binaries land non-executable and fail to spawn. Either use `externalBin`
  (handles +x and target-triple suffixing) or add a post-bundle `chmod +x` step. Decide early — it
  changes how you wire the sidecars into the config.

## 5. Signing + notarization (owner decision: Apple Developer Program, $99/yr)
Without this, Gatekeeper blocks the app ("unidentified developer"). Requirements:
- An **Apple Developer ID Application** cert (needs the paid program).
- Sign the app AND **every bundled Mach-O** — the third-party ffmpeg/whisper/sherpa binaries and all
  their dylibs must be signed with the same Developer ID and the **hardened runtime**
  (`codesign --force --options runtime --sign "Developer ID Application: …"` on each, then the `.app`).
  Notarization rejects any unsigned/un-hardened binary in the bundle.
- Notarize + staple: `xcrun notarytool submit … --wait` then `xcrun stapler staple`.
- Tauri has built-in macOS signing/notarization config (`APPLE_CERTIFICATE`, `APPLE_ID`,
  `APPLE_PASSWORD`/`APPLE_API_KEY`, `APPLE_SIGNING_IDENTITY`). Wire it like the (inert) Windows signing
  step in `.github/workflows/release.yml` — leave it gated until the account exists.

## The Windows lessons that WILL recur (save yourself the debugging)
- **Run the actual `.app`, not just the CLI.** On Windows the missing `DirectML.dll` crashed the GUI
  on launch, but every headless/CLI check passed (the CLI ran from the build dir where a symlink
  resolved). The onnxruntime `.dylib` bundling here is the identical trap — a fresh `.app` on a clean
  Mac is the only test that catches it. Install and launch before declaring done.
- **Provision before you build.** `vendor/` is gitignored; run the fetch scripts first or the bundle
  step fails on a missing resource glob.
- **ort drops its runtime lib next to the target as a per-user artifact** (a symlink on Windows). The
  bundler won't follow it. Stage the *real* dylib into `vendor/` and add it to `bundle.resources`
  (that's what `scripts/stage-directml.ps1` does on Windows — write the macOS analog).
- **Per-lane green ≠ integrated green.** Run the FULL workspace test after any cross-crate change; and
  the audio-quality gate (DNSMOS) says nothing about whether a build launches — verify both.

## Toolchain on the Mac
Rust (rustup — cargo is normally on PATH, unlike the Windows box), Node/npm, and **Xcode Command Line
Tools** (`xcode-select --install`) for `clang`, `codesign`, `notarytool`. No MSVC. The audio engine,
whisper, and eval all run headless the same way.

## Definition of done for M6
A signed, notarized universal `.dmg` that, installed on a clean Mac with no dev tools and no env
vars, launches and runs the same flow as Windows: drop → Master (denoise + loudness) → export every
format → transcribe → diarize → chapters → Clip Studio. Then run `docs/TEST-PLAN.md` (adapted) on a
real Mac and record results. After that: the two ports are at parity and it's one release.

## Still-open owner decisions that touch M6
- Apple Developer Program ($99/yr) — required for signing/notarization above.
- Final product name (the app shows "ANVIL"; repo is `anvil` — rename if it changes).
- Code-signing approach on Windows (SignPath OSS vs Azure Trusted Signing) — parallel decision.
