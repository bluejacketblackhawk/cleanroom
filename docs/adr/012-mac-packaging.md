# ADR-012: macOS Packaging (per-arch `.app`/`.dmg`, sidecar layout, exec bit, shared onnxruntime)

- Status: Accepted (packaging/layout + Developer ID signing implemented); notarization pending a
  one-time owner credential (`xcrun notarytool store-credentials anvil-notary`)
- Date: 2026-07-15
- Source: handoff/08-MAC.md § 3–4, STATE.md § M6 (S2/S3/S4), ADR-006 (platform layer target matrix),
  ADR-011 (Windows packaging — the sibling this mirrors), owner directive 2026-07-15 (6-artifact ship)

## Context

M6 ports Cleanroom to macOS. The engine is already cross-platform; what remained was the *bundle*: a
`.app`/`.dmg` that, on a clean Mac with no dev tools and no env vars, resolves the three sidecars
(ffmpeg, whisper.cpp, sherpa-onnx) and the diarization models the same way the Windows installer
does. Two macOS-specific facts shape every decision here:

1. In a `.app` the executable is at `Contents/MacOS/<exe>` but Tauri stages `bundle.resources` under
   `Contents/Resources/`, so the sidecars are **not** next to the binary (they are one `..` away).
2. Tauri's resource copy does not reliably preserve the Unix **executable bit**, and macOS will not
   spawn a non-`+x` binary — the exact class of "passes every CLI check, dies in the packaged app"
   bug that `DirectML.dll` was on Windows (handoff/08-MAC.md §4).

## Decision

**Per-arch artifacts, not universal2.** ADR-006's target matrix said "macOS 12+ universal2". We ship
**two single-arch bundles instead** — `mac-arm64` and `mac-x64` — per the owner's 2026-07-15
directive to ship all **6 artifacts** (win-x64 nsis+portable, win-arm64 nsis+portable, mac-arm64 dmg,
mac-x64 dmg). Rationale: the sidecars are already vendored and hash-pinned **per arch**
(`scripts/{ffmpeg,whisper,sherpa}-pin.json` `targets` maps), so a per-arch bundle has simpler, exact
provenance (one pinned binary per file, not a `lipo`-fattened pair whose two halves need separate
audit) and matches how `anvil-media`/`anvil-asr` already pin. universal2 would have re-fattened
binaries we deliberately keep thin. This supersedes ADR-006's universal2 line for the *bundle*; the
source still compiles for both arches from one tree.

**The Resources layout contract.** The bundle is built to serve the engine's read-only `locate()`
search path (`crates/anvil-media/src/sidecar.rs`, `crates/anvil-asr/src/{sidecar,diarize,model}.rs`),
which on macOS checks `../Resources/<name>/` relative to the exe. The bundle therefore lays down,
under `Contents/Resources/`:

| Path | Resolved by |
|---|---|
| `ffmpeg/ffmpeg` | `FfmpegSidecar::locate` → `../Resources/ffmpeg/ffmpeg` |
| `whisper/whisper-cli` + its 6 `libggml*/libwhisper` dylibs (flat; rpath `@loader_path`) | `WhisperSidecar::locate` → `../Resources/whisper/whisper-cli` |
| `sherpa/bin/<exe>` + `sherpa/lib/libonnxruntime.1.17.1.dylib` (rpath `@loader_path/../lib`) | `DiarizeSidecar::locate` → `../Resources/sherpa/bin/<exe>` |
| `models/{pyannote-segmentation-3-0,nemo_en_titanet_small}.onnx` | `anvil_asr::model::models_dirs` → `../Resources/models` |

A `beforeBundleCommand` (`scripts/stage-mac-sidecars.sh`, run by Tauri) copies the correct per-arch
tree out of `vendor/*/macos-<arch>/` into a **fixed** staging dir (`vendor/bundle-stage/macos/`) in
exactly this shape, so the *static* `tauri.macos.conf.json` can map stable paths without knowing the
arch. The arch comes from `TAURI_ENV_ARCH` (the hook variable Tauri actually exports — **not**
`TAURI_ENV_TARGET_TRIPLE`, which is not passed to hooks) or an explicit `$1`. Every staged file is
sha256-verified against the pins before it is trusted; a mismatch or missing vendor dir fails the
build.

**Exec-bit re-assertion.** `externalBin` (which handles `+x` for us) can't express the sherpa
`bin/`+`lib/` structure or the `../Resources/` layout, so sidecars ship as `bundle.resources`. We
therefore **re-assert `+x` at stage time** (`chmod +x` in the staging dir, the source of Tauri's
copy) and **re-verify it on the finished `.app`** (`apps/desktop/scripts/verify-mac-bundle.mjs`,
run post-bundle). The verifier is the Mac analogue of `release.yml`'s DirectML gate: it checks every
sidecar is present at its `locate()` path, is executable, and **spawns env-free** (`env` stripped —
which is simultaneously the dylib/rpath **load** check, since a missing onnxruntime surfaces as a
dyld abort, not a silent pass), plus that the demo file and the Info.plist keys are present. It exits
nonzero on any failure so CI catches a regression, never a user.

**One shared onnxruntime dylib.** `ort` (in-process DeepFilterNet3) is built `load-dynamic` on Intel
macOS — `ort` ships no prebuilt x86_64-darwin binary, so it resolves onnxruntime at run time from
`ORT_DYLIB_PATH` (`crates/anvil-ai/Cargo.toml`). Rather than bundle a second onnxruntime, Cleanroom
reuses the **one** the sherpa sidecar already carries (`sherpa/lib/libonnxruntime.1.17.1.dylib`,
Microsoft's universal2 1.17.1, which covers x86_64). `apps/desktop/src-tauri/src/lib.rs` sets
`ORT_DYLIB_PATH` to that copy at launch — `cfg(all(target_os="macos", target_arch="x86_64"))`, first
thing in `run()`, before any session or thread — unless a developer already set it. arm64 links ort
statically (CoreML-capable) and needs no dylib. This `cfg` lives in the desktop shell, not an engine
crate: ADR-006's "cfg only in `platform`" rule scopes the engine, and the shell already carries
platform-conditional code (`windows_subsystem`, `mobile` entry point).

**Developer ID signed; notarization is one owner command away.** Gatekeeper needs two things for a
no-warning launch: a Developer ID Application signature with the **hardened runtime** on *every*
Mach-O, and an Apple **notarization** ticket. `apps/desktop/scripts/sign-mac.mjs` implements both
halves — the signing half runs in the release lane today (against the real Developer ID, team
**7Y39A984XL**), the notarization half waits only on a one-time credential the owner stores.

*Why a post-bundle script, not `bundle.macOS.signingIdentity`.* The three sidecars ship as
`bundle.resources` — loose Mach-Os under `Contents/Resources/` (ffmpeg, whisper-cli + its 6 dylibs,
the sherpa exe + onnxruntime), **not** under `Contents/Frameworks/`, and the bundle has no Frameworks
dir at all. Tauri's bundle-time signing (what setting `signingIdentity` triggers) signs the main
executable and *seals* those resource files by hash, but does **not** give each nested Mach-O its own
hardened-runtime Developer ID signature — so the bundle would notarize-**reject** on the first
sidecar (Apple's notary checks every Mach-O). Fixing that needs the inside-out discipline Tauri's
single pass does not do, and `--deep` (the only recursive `codesign` mode) is Apple-documented as the
**wrong** tool for signing a distributable app (it re-signs nested code with the outer
options/identity and skips per-item entitlements). So `tauri.macos.conf.json` deliberately carries
**no** `signingIdentity`: Tauri emits an ad-hoc bundle and `sign-mac.mjs` re-signs everything after.
It still carries the `entitlements` (mic + library-validation opt-out) and `minimumSystemVersion`,
which the signing pass applies to the `.app`.

*The inside-out order (never `codesign --deep` to sign).* `sign-mac.mjs sign` signs **every bundled
Mach-O individually first** — discovered by Mach-O magic under `Contents/Resources/` (10 files: the
sherpa exe + its sibling onnxruntime dylib, whisper-cli + its 6 dylibs, ffmpeg) — each with
`--force --options runtime --timestamp` and **no** entitlements, **then the `.app` last** with
`entitlements.mac.plist`. Sidecars need no entitlements of their own: after the sherpa exe *and*
onnxruntime are re-signed with the **same** team identity, hardened-runtime library validation
accepts the (now same-team) dylib — proven by `verify-mac-bundle.mjs` spawning every signed sidecar
**env-free** on both arches (arm64 native, x86_64 via Rosetta). The identity is not hardcoded beyond
the team ID: `sign-mac.mjs` auto-detects the first "Developer ID Application" from
`security find-identity`, overridable via `$CLEANROOM_SIGN_IDENTITY` or `--identity`.

*Reading spctl correctly.* After the signing half and **before** notarization, `spctl --assess`
**must** report *rejected — "Unnotarized Developer ID"* on both the `.app` (`--type execute`) and the
`.dmg` (`--type open --context context:primary-signature`). That is the **expected, correct**
signing-half state: the code is validly Developer-ID signed but Gatekeeper will not admit it without
a notarization ticket. `sign-mac.mjs verify` labels this outcome so it is never misread as a failure;
it flips to *accepted — "Notarized Developer ID"* after notarize + staple.

*The one remaining owner step.* Create the notarytool credential once, then run the notarize+staple
tail — or the whole one-shot:

```
xcrun notarytool store-credentials anvil-notary --apple-id <id> --team-id 7Y39A984XL --password <app-specific-pw>
node apps/desktop/scripts/sign-mac.mjs release-mac --target all --profile anvil-notary
#   or just the tail, on the already-signed DMGs:
node apps/desktop/scripts/sign-mac.mjs notarize --profile anvil-notary --wait --target all
node apps/desktop/scripts/sign-mac.mjs staple   --target all
```

`sign-mac.mjs` gates on the profile: until it exists, `release-mac` stops after the signed DMG and
prints exactly this; once it exists the same command notarizes, staples the DMG (and best-effort the
`.app`), and re-verifies to the *accepted* state. Notarizing the DMG covers the app inside it, so a
user dragging Cleanroom.app to /Applications gets a clean first launch.

## The content-hash pin — reconciling the hash gate with code signing

The engine refuses to run a sidecar whose hash does not match a compiled-in pin (a licence control
for ffmpeg — `anvil-media`; a provisioning/bundle contract for whisper + sherpa — `anvil-asr`).
Signing broke that gate: `codesign` **rewrites the Mach-O** when it re-signs (Developer ID replaces
the ad-hoc signature), so the shipped, signed `ffmpeg`/`whisper-cli`/sherpa exe no longer match the
raw sha256 recorded from the vendored (ad-hoc-signed) binary. The symptom was a hard, shipped-only
failure — `ffmpeg sidecar hash mismatch: expected 350a7053… got e7926266…` — that killed **every
ffmpeg-dependent feature** (all MP3/encodes, video, clips, transcribe staging) in the signed `.app`
while WAV export (native writer) still worked.

**Decision: pin the CONTENT, not the raw file.** The gate compares a signing-independent *content
hash* — the Mach-O with its code-signature blob and the three header fields describing that blob's
size neutralised — so identity is stable across ad-hoc ↔ Developer ID ↔ any re-sign, while any real
content tamper is still refused (all code, data, symbol tables, and load commands remain hashed).

*The hash* (`macho_content_sha256`, `std`-only, no new dependency):
- **Thin 64-bit Mach-O** with an `LC_CODE_SIGNATURE`: sha256 over `[0, dataoff)` — header + load
  commands + real `__LINKEDIT` — with `__LINKEDIT.vmsize`, `__LINKEDIT.filesize`, and
  `LC_CODE_SIGNATURE.datasize` zeroed (those three, and only those three, differ between an ad-hoc
  and a Developer-ID signature of the same program), and the trailing signature blob excluded.
  With NO signature, the content hash is the raw sha256 of the whole file — so unsigned/dev/Linux
  binaries keep identical semantics.
- **FAT / universal** (sherpa's `universal2`): sha256 over the *stable* fat fields (`magic`,
  `nfat_arch`, and per-arch `cputype`/`cpusubtype`/`align`) then each slice's own content hash, in
  fat-header order. The per-arch `offset`/`size` are excluded — they shift when a slice is
  re-signed, and they only select which bytes each slice hash already covers, so a tampered
  offset/size still changes a slice hash or fails a bounds check.
- **Not for PE/Windows.** Windows pins stay raw sha256; Authenticode will get the same treatment
  when Windows signing lands. Selection is by the runtime target (`macos-*` → content, else raw).

The macOS pins therefore carry BOTH hashes: `binary_sha256` (raw — the provisioning gate the
fetch/build/stage scripts and the vendored-artifact tests still enforce, unchanged) and
`content_sha256` (the run-time gate). The vendored binary's content hash equals the shipped signed
copy's — the invariant the whole design rests on — and that is proven, not assumed (see below).
sherpa is the one subtlety: its upstream `universal2` **x86_64 slice ships unsigned**, but codesign
signs every slice at packaging time, so `content_sha256` is recorded from a **fully** (ad-hoc)
signed reference — which is exactly what the Developer-ID `.app` hashes to.

*Where the code lives.* The parser is in `anvil-media` (`crates/anvil-media/src/sidecar.rs`) and
**duplicated privately** in `anvil-asr` (`crates/anvil-asr/src/pin.rs`). `anvil-asr` deliberately
does **not** depend on `anvil-media` — sharing ~150 lines would drag the entire media lane
(symphonia/rubato/lofty/ffmpeg) into every crate that builds ASR, a cost far larger than the
duplication. Both copies are independently unit-tested against hand-built synthetic Mach-Os
(signed/unsigned/fat, plus truncated and hostile inputs that must error, not panic).

*Verification split (why the pin gate is a Rust test, not a Node re-implementation).*
`verify-mac-bundle.mjs` runs `codesign --verify --strict` per sidecar — proving each signed Mach-O
is untampered *since signing* (a hard failure) — but it does **not** re-derive the content hash: a
second Mach-O parser in Node would be a source of truth that could silently drift from the engine's.
The authoritative bundle-vs-pin proof is instead a **mac-gated Rust integration test** run against
the built `.app` with the exact code the gate uses:
`anvil-media`'s `signed_bundle_ffmpeg_content_hash_matches_the_pin` and `anvil-asr`'s
`signed_bundle_content_hash_matches_the_pins` each assert, per arch, that the vendored binary's raw
hash matches its provision pin AND that the fully-signed / Developer-ID-signed copy's content hash
matches `content_sha256` — even though the raw hashes differ. `sign-mac.mjs verify` prints the exact
commands so the release operator runs them after signing. Both tests skip cleanly when neither the
vendored binary nor a signed `.app` is present, so `cargo test` stays green anywhere.

## Consequences

**Enables:** a clean-Mac, env-free install that runs the full flow (master → export → transcribe →
diarize → Clip Studio) on both arm64 and Intel; a build that fails loudly (`verify-mac-bundle.mjs`)
if a sidecar, its exec bit, its dylib, or an Info.plist key ever regresses; Developer ID signing +
notarization via `sign-mac.mjs` (signing done inside-out on both arches; notarization one owner
command away).

**Constrains:**
- Anyone changing a sidecar's on-disk layout must change **both** the engine `locate()` candidate
  **and** `stage-mac-sidecars.sh` + `tauri.macos.conf.json` in lockstep, or a clean-Mac launch
  silently loses that sidecar. `verify-mac-bundle.mjs` is the guard — keep it invoked from the
  release lane.
- The staged binaries' hashes are pinned per host build (ffmpeg/whisper are from-source). Rebuilding
  a sidecar means re-recording BOTH its `binary_sha256` (raw — the stage step and provisioning tests)
  AND, on macOS, its `content_sha256` (the run-time gate) in `scripts/*-pin.json` and the code-side
  pin tables, or the stage step / the mac-gated content-hash test refuses it. The content hash is
  computed from a fully (ad-hoc) signed copy, so it does not change when the release lane later
  re-signs with Developer ID — that stability is the point.
- Release binaries are Developer-ID signed with the hardened runtime on every bundled Mach-O
  (`apps/desktop/scripts/sign-mac.mjs sign`, run in the release lane), which is why re-signing is
  mandatory: the sherpa binary + onnxruntime ship only linker-signed upstream, so `sign-mac.mjs`
  replaces those ad-hoc signatures inside-out. The last mile to a no-warning launch is notarization —
  one owner command (`notarytool store-credentials anvil-notary` → `sign-mac.mjs release-mac`). Until
  that credential exists the signed builds still launch via **right-click → Open**, and `spctl`
  reports the expected "Unnotarized Developer ID" state (NOT a failure — see the signing section).
- `sign-mac.mjs` is the signing counterpart to `verify-mac-bundle.mjs`, which it re-invokes on the
  signed `.app`: signatures must not break the env-free sidecar spawn (they do not — same-team library
  validation), so the release lane runs `sign` → `verify` → `dmg` and, once the credential exists,
  `notarize` → `staple`. Keep both invoked from the release lane.
- The per-arch decision means the release matrix builds macOS twice (once per arch). In CI the
  Intel *app* builds natively on the `macos-15-intel` runner (catches Intel-specific issues the
  compile can't), while the *sidecars* for both arches are always provisioned on an Apple-Silicon
  host (`build-ffmpeg-macos.sh` is arm64-host-only and cross-compiles x86_64) and handed to the
  bundle legs as a verified artifact. Local development on an arm64 Mac cross-builds both.
