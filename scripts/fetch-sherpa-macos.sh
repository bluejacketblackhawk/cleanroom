#!/usr/bin/env bash
#
# fetch-sherpa-macos.sh — provision the pinned sherpa-onnx speaker-diarization sidecar for
# macOS. The macOS analogue of scripts/fetch-sherpa.ps1.
#
# Unlike whisper.cpp (no macOS release, built from source), sherpa-onnx DOES publish an official
# macOS build: a single `osx-universal2-shared` tarball whose binary is a fat arm64+x86_64
# Mach-O. So we download + verify it (never build), and stage the SAME bytes into both per-arch
# vendor dirs for bundle symmetry (identical hashes — one artifact, two targets).
#
# What it does:
#   1. downloads the pinned universal2 archive and verifies its sha256 (hard fail on mismatch),
#   2. extracts the diarization binary + EXACTLY the dylibs `otool -L` says it needs from lib/
#      (empirically: only libonnxruntime.<ver>.dylib — the sherpa C++ core is statically linked
#      into the exe, and macOS onnxruntime has no providers-shared stub, unlike Windows),
#   3. stages them PRESERVING the bin/ + lib/ sibling structure (the binary's baked rpath is
#      @loader_path/../lib, so the tree runs with the environment stripped and needs no fixup),
#      into BOTH vendor/sherpa/macos-aarch64/ and vendor/sherpa/macos-x86_64/,
#   4. ships the Apache-2.0 (sherpa) + MIT (onnxruntime) licence texts next to the binary,
#   5. proves each staged tree runs env-free, and (if the diarization models are already present
#      in vendor/models) attempts a model-load run — full DER is S7's job, not this script's,
#   6. computes + records every staged file's sha256.
#
# DiarizeSidecar::locate() resolves the binary at sherpa/bin/<exe> next to the app (or
# ../Resources/sherpa/bin/<exe> inside a .app). Record the printed sha256s into
# scripts/sherpa-pin.json .targets.<target>.binary and crates/anvil-asr/src/pin.rs SHERPA_PINS.
#
# Usage:
#   scripts/fetch-sherpa-macos.sh            # download + verify + stage both targets + smoke
#   scripts/fetch-sherpa-macos.sh --force    # re-download even if cached
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PIN="$SCRIPT_DIR/sherpa-pin.json"
LICENSE_DIR="$SCRIPT_DIR/licenses"
CACHE_DIR="$REPO_ROOT/.cache/sherpa"
MODELS_DIR="$REPO_ROOT/vendor/models"

EXE="sherpa-onnx-offline-speaker-diarization"
TARGETS="macos-aarch64 macos-x86_64"

# The pin is the source of truth for the URL + sha256 (mac entries share one universal2 artifact,
# so either target's binary block carries the same values — read aarch64's).
URL="$(jq -r '.targets."macos-aarch64".binary.source_url' "$PIN")"
ARCHIVE_SHA="$(jq -r '.targets."macos-aarch64".binary.archive_sha256' "$PIN")"
# bash 3.2 (macOS system bash) has no mapfile — read arrays with a while loop.
LICENSE_FILES=()
while IFS= read -r _lf; do [ -n "$_lf" ] && LICENSE_FILES+=("$_lf"); done \
  < <(jq -r '.targets."macos-aarch64".binary.license_files[]?' "$PIN" 2>/dev/null || true)

FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

say_ok()   { printf '  \033[32mok\033[0m  %s\n' "$*"; }
say_step() { printf '\033[36m%s\033[0m\n' "$*"; }
die()      { printf '\033[31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }
sha256()   { shasum -a 256 "$1" | awk '{print $1}'; }

# --- 1. download + verify ---------------------------------------------------------------------
mkdir -p "$CACHE_DIR"
ARCHIVE="$CACHE_DIR/$(basename "$URL")"
if [ "$FORCE" = 1 ] || [ ! -f "$ARCHIVE" ]; then
  say_step "Downloading sherpa-onnx universal2 archive"
  curl -fL --retry 5 --retry-all-errors --retry-delay 2 -o "$ARCHIVE" "$URL"
fi
got="$(sha256 "$ARCHIVE")"
[ "$got" = "$ARCHIVE_SHA" ] || die "sherpa archive sha256 mismatch (expected $ARCHIVE_SHA, got $got). Refusing to install an unverified sherpa-onnx."
say_ok "sherpa archive sha256 $got ($(stat -f%z "$ARCHIVE") bytes)"

# --- 2. extract, resolve the exact dylib set --------------------------------------------------
STAGING="$CACHE_DIR/extract"
rm -rf "$STAGING"; mkdir -p "$STAGING"
tar -xjf "$ARCHIVE" -C "$STAGING"
ROOT="$STAGING/$(ls "$STAGING")"
BIN_SRC="$ROOT/bin/$EXE"
[ -f "$BIN_SRC" ] || die "diarization binary not found in archive"

# The bundle needs the binary + exactly the @rpath dylibs otool reports (resolved out of lib/).
# Recorded EXACTLY rather than globbed, so a surprise extra dependency is caught, not hidden.
NEEDED=()
while IFS= read -r _n; do [ -n "$_n" ] && NEEDED+=("$_n"); done \
  < <(otool -L "$BIN_SRC" | tail -n +2 | awk '{print $1}' | grep '^@rpath/' | sed 's#^@rpath/##' | sort -u)
[ "${#NEEDED[@]}" -gt 0 ] || die "no @rpath dependency found on $EXE — the layout changed, re-audit"
say_step "otool -L truth: $EXE needs ${NEEDED[*]} from lib/ (+ system libSystem/libc++)"
for n in "${NEEDED[@]}"; do
  [ -f "$ROOT/lib/$n" ] || die "otool says $EXE needs $n but it is not in lib/ — layout changed, re-audit"
done

# --- 3+4. stage identical content into every target (preserve bin/ + lib/ siblings) ----------
stage_one() {
  local target="$1"
  local dest="$REPO_ROOT/vendor/sherpa/$target"
  rm -rf "$dest"; mkdir -p "$dest/bin" "$dest/lib"
  # ditto preserves the fat Mach-O + its (adhoc linker-signed) signature faithfully.
  ditto "$BIN_SRC" "$dest/bin/$EXE"; chmod +x "$dest/bin/$EXE"
  local n; for n in "${NEEDED[@]}"; do ditto "$ROOT/lib/$n" "$dest/lib/$n"; done
  local lf; for lf in "${LICENSE_FILES[@]+"${LICENSE_FILES[@]}"}"; do
    [ -f "$LICENSE_DIR/$lf" ] || die "licence text $LICENSE_DIR/$lf missing — a redistributed binary must ship its notice"
    cp "$LICENSE_DIR/$lf" "$dest/$lf"
  done

  # Prove the staged tree runs with the environment fully stripped, from its own location.
  local usage; usage="$(cd "$dest" && env -i "./bin/$EXE" --help 2>&1 || true)"
  echo "$usage" | grep -qi 'diariz' || die "$target: staged $EXE did not print diarization usage env-free"
  say_ok "$target: runs env-free (bin/$EXE + lib/${NEEDED[*]})"

  ( cd "$dest" && shasum -a 256 "bin/$EXE" lib/*.dylib > SHA256SUMS )
  say_ok "$target staged:"
  ( cd "$dest" && shasum -a 256 "bin/$EXE" lib/*.dylib | sed 's/^/     /' )
}

for t in $TARGETS; do stage_one "$t"; done

# --- 5. diarization model-load smoke (only if the models are already provisioned) -------------
SEG="$MODELS_DIR/sherpa-onnx-pyannote-segmentation-3-0.onnx"
EMB="$MODELS_DIR/nemo_en_titanet_small.onnx"
DEST_AARCH="$REPO_ROOT/vendor/sherpa/macos-aarch64"
if [ -f "$SEG" ] && [ -f "$EMB" ]; then
  say_step "Diarization model-load smoke (arm64): a 5 s two-tone WAV won't diarize meaningfully — this only proves the models load; DER is S7"
  WAV="$(mktemp -u).wav"
  python3 - "$WAV" <<'PY'
import wave, struct, math, sys
w=wave.open(sys.argv[1],'wb'); w.setnchannels(1); w.setsampwidth(2); w.setframerate(16000)
fr=[]
for t in range(80000):
    f=220 if t<40000 else 440
    fr.append(struct.pack('<h', int(6000*math.sin(2*math.pi*f*t/16000))))
w.writeframes(b''.join(fr)); w.close()
PY
  out="$(env -i "$DEST_AARCH/bin/$EXE" \
      --segmentation.pyannote-model="$SEG" --embedding.model="$EMB" \
      --clustering.num-clusters=2 "$WAV" 2>&1 || true)"
  if echo "$out" | grep -qiE 'Started|Segmentation|speaker_|Elapsed|Num speakers'; then
    say_ok "sherpa loaded both ONNX models and ran the pipeline (output is not a DER claim)"
  else
    printf '  \033[33m!!\033[0m  model-load run produced no recognisable pipeline output:\n%s\n' "$(echo "$out" | tail -4)"
  fi
  rm -f "$WAV"
else
  echo "  (diarization models not in vendor/models — skipping model-load smoke; run scripts/fetch-sherpa.ps1's model step or provision them, DER is S7)"
fi

echo
say_ok "sherpa-onnx $(jq -r '.targets."macos-aarch64".version' "$PIN") diarization sidecar staged for: $TARGETS"
echo "Record the SHA256SUMS above into scripts/sherpa-pin.json (.targets.<target>.binary) and"
echo "crates/anvil-asr/src/pin.rs (SHERPA_PINS). For dev: export ANVIL_DIARIZE=<dir>/bin/$EXE"
