#!/usr/bin/env bash
#
# stage-mac-sidecars.sh — stage the pinned macOS sidecars for ONE architecture into a FIXED
# directory so the STATIC tauri.macos.conf.json can reference stable, arch-independent paths.
# The macOS analogue of scripts/stage-directml.ps1; run by Tauri's `beforeBundleCommand`
# during `npm run tauri build` (see tauri.macos.conf.json).
#
# WHY a stage dir. tauri.macos.conf.json's `bundle.resources` is static JSON — it cannot know
# whether this build targets aarch64 or x86_64, so it cannot point at `vendor/*/macos-<arch>/`
# directly. This script copies the correct per-arch tree out of `vendor/{ffmpeg,whisper,sherpa,
# models}/` into `vendor/bundle-stage/macos/` in the EXACT shape the app's `locate()` search
# path expects inside the `.app`, and the config maps that fixed dir into `Contents/Resources/`.
#
# THE LAYOUT CONTRACT (must match crates/anvil-media/src/sidecar.rs + crates/anvil-asr/src/{
# sidecar,diarize,model}.rs `locate()` — the bundle serves THEM, they are read-only):
#
#   vendor/bundle-stage/macos/
#     ffmpeg/ffmpeg                                  -> Contents/Resources/ffmpeg/ffmpeg
#     ffmpeg/*LICENSE*.txt                              (LGPL redistribution notices)
#     whisper/whisper-cli                            -> Contents/Resources/whisper/whisper-cli
#     whisper/lib*.dylib   (6 dylibs, rpath @loader_path — resolved next to the binary)
#     whisper/LICENSE.txt
#     sherpa/bin/sherpa-onnx-offline-speaker-diarization
#                                                    -> Contents/Resources/sherpa/bin/<exe>
#     sherpa/lib/libonnxruntime.1.17.1.dylib   (rpath @loader_path/../lib — resolved from lib/)
#     sherpa/*LICENSE*.txt
#     models/sherpa-onnx-pyannote-segmentation-3-0.onnx   -> Contents/Resources/models/*.onnx
#     models/nemo_en_titanet_small.onnx                   (the two diarization models)
#     models/*LICENSE*.txt models/*ATTRIBUTION*.txt models/*legalcode*.txt
#
# (whisper ggml weights are NOT bundled — the Models screen downloads them on demand; only the
# two diarization ONNX models ship so diarization works out of the box.)
#
# ARCH SELECTION. In priority order:
#   1. $1                      — explicit selector: aarch64|arm64|x86_64|x64|macos-<arch>|<triple>
#   2. $TAURI_ENV_TARGET_TRIPLE — a full triple, if Tauri ever exports it to hooks
#   3. $TAURI_ENV_ARCH          — the documented hook var (values `aarch64` / `x86_64`); this is
#                                 what a real `tauri build --target <triple>` actually provides.
# NOTE: contrary to a common assumption, Tauri does NOT export TAURI_ENV_TARGET_TRIPLE to
# beforeBundleCommand — only TAURI_ENV_{PLATFORM,ARCH,FAMILY,PLATFORM_VERSION,PLATFORM_TYPE,DEBUG}
# (tauri-utils 2.9 config docs). TAURI_ENV_ARCH is the reliable one, and it maps 1:1 onto the
# `macos-<arch>` vendor dirs.
#
# INTEGRITY. Every staged Mach-O and model is sha256-verified against scripts/{ffmpeg,whisper,
# sherpa}-pin.json before it is trusted (the same pins the fetch/build scripts recorded and, for
# ffmpeg, the same hash anvil-media enforces at RUN time). A missing vendor dir or a hash mismatch
# fails the build HERE — we never bundle an unaudited or wrong-arch binary. `ditto` is used for the
# copy so each sidecar's existing ad-hoc code signature is preserved faithfully.
#
# EXEC BIT. Tauri's resource copy is the known trap (handoff/08-MAC.md §4): bundled resources can
# land without the Unix +x bit. We re-assert +x on the three executables HERE (so the source of the
# copy is correct) and scripts/verify-mac-bundle.mjs re-checks it on the built `.app` post-bundle.
#
# USAGE:
#   scripts/stage-mac-sidecars.sh aarch64          # stage the arm64 tree
#   scripts/stage-mac-sidecars.sh x86_64-apple-darwin
#   TAURI_ENV_ARCH=x86_64 scripts/stage-mac-sidecars.sh   # as Tauri invokes it
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
VENDOR="$REPO_ROOT/vendor"
STAGE="$VENDOR/bundle-stage/macos"

FFMPEG_PIN="$SCRIPT_DIR/ffmpeg-pin.json"
WHISPER_PIN="$SCRIPT_DIR/whisper-pin.json"
SHERPA_PIN="$SCRIPT_DIR/sherpa-pin.json"

SHERPA_EXE="sherpa-onnx-offline-speaker-diarization"

say_ok()   { printf '  \033[32mok\033[0m  %s\n' "$*"; }
say_step() { printf '\033[36m%s\033[0m\n' "$*"; }
die()      { printf '\033[31mERROR (stage-mac-sidecars):\033[0m %s\n' "$*" >&2; exit 1; }
sha256()   { shasum -a 256 "$1" | awk '{print $1}'; }

# jq is required (present at /usr/bin/jq on the build machine); the pins are the integrity source.
command -v jq >/dev/null 2>&1 || die "jq not found — it reads the sha256 pins that gate every staged binary"

# --- resolve the target arch ------------------------------------------------------------------
normalize_arch() {
  case "$1" in
    *aarch64*|*arm64*)        echo "aarch64" ;;
    *x86_64*|*x64*|*amd64*)   echo "x86_64" ;;
    *)                        echo "" ;;
  esac
}

# This script is macOS-only; if Tauri tells us the platform and it is not darwin, refuse loudly
# rather than stage mac binaries into a non-mac build.
if [ -n "${TAURI_ENV_PLATFORM:-}" ] && [ "${TAURI_ENV_PLATFORM}" != "darwin" ]; then
  die "TAURI_ENV_PLATFORM=${TAURI_ENV_PLATFORM} is not darwin — this stages macOS sidecars only"
fi

RAW_SELECTOR="${1:-${TAURI_ENV_TARGET_TRIPLE:-${TAURI_ENV_ARCH:-}}}"
ARCH="$(normalize_arch "$RAW_SELECTOR")"
[ -n "$ARCH" ] || die "could not determine target arch from '\$1'='${1:-}' / TAURI_ENV_TARGET_TRIPLE='${TAURI_ENV_TARGET_TRIPLE:-}' / TAURI_ENV_ARCH='${TAURI_ENV_ARCH:-}'. Pass aarch64 or x86_64 explicitly."
TARGET="macos-$ARCH"
say_step "Staging macOS sidecars for $TARGET into $STAGE"

FFMPEG_SRC="$VENDOR/ffmpeg/$TARGET"
WHISPER_SRC="$VENDOR/whisper/$TARGET"
SHERPA_SRC="$VENDOR/sherpa/$TARGET"
MODELS_SRC="$VENDOR/models"
for d in "$FFMPEG_SRC" "$WHISPER_SRC" "$SHERPA_SRC" "$MODELS_SRC"; do
  [ -d "$d" ] || die "vendor dir missing: $d — run the provisioners first (scripts/build-ffmpeg-macos.sh, scripts/build-whisper-macos.sh, scripts/fetch-sherpa-macos.sh)"
done

# `lipo -archs` sanity: the single-arch builds (ffmpeg, whisper) must actually be the arch we were
# asked to stage. Catches a mis-populated vendor dir before the (also-checked) sha256 does, with a
# clearer message. sherpa is one universal2 artifact (both arches), so it is exempt.
# NOTE: `lipo` reports Mach-O arch names (`arm64`, `x86_64`), NOT the Rust names (`aarch64`,
# `x86_64`) used for the vendor dirs — translate before comparing.
case "$ARCH" in
  aarch64) MACHO_ARCH="arm64" ;;
  x86_64)  MACHO_ARCH="x86_64" ;;
esac
assert_arch() { # $1=binary
  command -v lipo >/dev/null 2>&1 || return 0
  local archs; archs="$(lipo -archs "$1" 2>/dev/null || true)"
  case " $archs " in
    *" $MACHO_ARCH "*) : ;;
    *) die "$1 is '$archs', not $MACHO_ARCH — vendor/*/$TARGET holds the wrong architecture" ;;
  esac
}

verify_sha() { # $1=file $2=expected-sha256
  [ -f "$1" ] || die "expected file not staged: $1"
  local got; got="$(sha256 "$1")"
  [ "$got" = "$2" ] || die "sha256 MISMATCH for $1
    expected $2 (scripts pin)
    got      $got
  The vendor tree is corrupt, stale, or the wrong arch. Refusing to bundle an unaudited sidecar."
}

# fresh stage every run (cheap; guarantees no cross-arch leftovers survive)
rm -rf "$STAGE"
mkdir -p "$STAGE/ffmpeg" "$STAGE/whisper" "$STAGE/sherpa/bin" "$STAGE/sherpa/lib" "$STAGE/models"

# --- ffmpeg (flat) ----------------------------------------------------------------------------
say_step "ffmpeg"
FFMPEG_SHA="$(jq -r --arg t "$TARGET" '.targets[$t].binary_sha256' "$FFMPEG_PIN")"
[ -n "$FFMPEG_SHA" ] && [ "$FFMPEG_SHA" != "null" ] || die "no ffmpeg binary_sha256 for $TARGET in $FFMPEG_PIN"
ditto "$FFMPEG_SRC/ffmpeg" "$STAGE/ffmpeg/ffmpeg"
chmod +x "$STAGE/ffmpeg/ffmpeg"
assert_arch "$STAGE/ffmpeg/ffmpeg"
verify_sha "$STAGE/ffmpeg/ffmpeg" "$FFMPEG_SHA"
say_ok "ffmpeg/ffmpeg (+x, sha256 pinned)"
# LGPL redistribution notices ship next to the binary (ffmpeg + its static codec deps).
n=0; for lic in "$FFMPEG_SRC"/*LICENSE*.txt; do [ -f "$lic" ] || continue; cp "$lic" "$STAGE/ffmpeg/"; n=$((n+1)); done
[ "$n" -gt 0 ] || die "no ffmpeg LICENSE text found in $FFMPEG_SRC — a redistributed LGPL binary MUST ship its licence"
say_ok "$n ffmpeg licence text(s)"

# --- whisper (flat: whisper-cli + its dylibs, rpath @loader_path) ------------------------------
say_step "whisper.cpp"
assert_arch "$WHISPER_SRC/whisper-cli"
# every member (binary + dylibs) is pinned by dest+sha256 in whisper-pin.json
while IFS=$'\t' read -r dest sha; do
  [ -n "$dest" ] || continue
  [ -f "$WHISPER_SRC/$dest" ] || die "whisper member '$dest' not in $WHISPER_SRC"
  ditto "$WHISPER_SRC/$dest" "$STAGE/whisper/$dest"
  verify_sha "$STAGE/whisper/$dest" "$sha"
done < <(jq -r --arg t "$TARGET" '.targets[$t].members[] | [.dest, .sha256] | @tsv' "$WHISPER_PIN")
chmod +x "$STAGE/whisper/whisper-cli"
say_ok "whisper-cli (+x) + $(ls "$STAGE"/whisper/*.dylib | wc -l | tr -d ' ') dylibs (all sha256 pinned)"
for lic in "$WHISPER_SRC"/*LICENSE*.txt; do [ -f "$lic" ] && cp "$lic" "$STAGE/whisper/"; done

# --- sherpa (bin/ + lib/ siblings, rpath @loader_path/../lib) ----------------------------------
say_step "sherpa-onnx"
# members carry their bin/ or lib/ prefix in `dest`; stage preserving that structure.
while IFS=$'\t' read -r dest sha; do
  [ -n "$dest" ] || continue
  [ -f "$SHERPA_SRC/$dest" ] || die "sherpa member '$dest' not in $SHERPA_SRC"
  mkdir -p "$STAGE/sherpa/$(dirname "$dest")"
  ditto "$SHERPA_SRC/$dest" "$STAGE/sherpa/$dest"
  verify_sha "$STAGE/sherpa/$dest" "$sha"
done < <(jq -r --arg t "$TARGET" '.targets[$t].binary.members[] | [.dest, .sha256] | @tsv' "$SHERPA_PIN")
chmod +x "$STAGE/sherpa/bin/$SHERPA_EXE"
[ -f "$STAGE/sherpa/lib/libonnxruntime.1.17.1.dylib" ] || die "onnxruntime dylib not staged under sherpa/lib/ — sherpa diarization AND (on Intel) in-process ort both need it"
say_ok "sherpa/bin/$SHERPA_EXE (+x) + sherpa/lib/libonnxruntime.1.17.1.dylib (sha256 pinned)"
for lic in "$SHERPA_SRC"/*LICENSE*.txt; do [ -f "$lic" ] && cp "$lic" "$STAGE/sherpa/"; done

# --- diarization models (the two default ONNX packs) ------------------------------------------
say_step "diarization models"
while IFS=$'\t' read -r dest sha; do
  [ -n "$dest" ] || continue
  [ -f "$MODELS_SRC/$dest" ] || die "model '$dest' not in $MODELS_SRC (run the sherpa model provisioning step)"
  ditto "$MODELS_SRC/$dest" "$STAGE/models/$dest"
  verify_sha "$STAGE/models/$dest" "$sha"
done < <(jq -r '.models[] | [.dest, .onnx_sha256] | @tsv' "$SHERPA_PIN")
say_ok "$(ls "$STAGE"/models/*.onnx | wc -l | tr -d ' ') diarization ONNX models (sha256 pinned)"
# model licence + the mandatory CC-BY-4.0 attribution for TitaNet ship alongside them.
for lic in "$MODELS_SRC"/*LICENSE*.txt "$MODELS_SRC"/*ATTRIBUTION*.txt "$MODELS_SRC"/*legalcode*.txt; do
  [ -f "$lic" ] && cp "$lic" "$STAGE/models/"
done

# --- manifest ---------------------------------------------------------------------------------
printf '%s\n' "$TARGET" > "$STAGE/.staged-arch"
( cd "$STAGE" && find . -type f \( -name '*.dylib' -o -name 'ffmpeg' -o -name 'whisper-cli' -o -name "$SHERPA_EXE" -o -name '*.onnx' \) -print0 \
    | xargs -0 shasum -a 256 > SHA256SUMS )
echo
say_ok "staged $TARGET — Contents/Resources layout ready for tauri.macos.conf.json bundle.resources"
