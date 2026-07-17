#!/usr/bin/env bash
#
# build-whisper-macos.sh — build the pinned MIT whisper.cpp CLI sidecar for macOS, per-arch,
# reproducibly and verifiably. The macOS analogue of scripts/fetch-whisper.ps1.
#
# whisper.cpp v1.9.1 ships ZERO macOS release binaries (only whisper-bin-x64.zip for Windows),
# so on macOS we build from source at the pinned tag. Cleanroom is MIT and runs whisper.cpp as a
# SIDECAR PROCESS (never linked), so the build stays MIT-clean: the default shared build, no
# GGML_STATIC (broken global -static on Apple), Metal + Accelerate for the perf.
#
# What it does:
#   1. ensures cmake exists (else downloads the sha256-pinned official CMake.app into ~/.local —
#      no Homebrew on this machine),
#   2. clones whisper.cpp at the pinned tag (scripts/whisper-pin.json .targets.*.source_ref),
#   3. builds whisper-cli SEPARATELY for arm64 and x86_64 — two single-arch builds, NOT universal
#      (Cleanroom bundles per-arch). GGML_NATIVE=OFF (critical: the default bakes -march=native, which
#      is non-portable in a shipped binary; OFF gives the portable baseline and Metal/Accelerate
#      carry the perf). Metal stays on by default and GGML_METAL_EMBED_LIBRARY is ON at this tag,
#      so the metallib is embedded in libggml-metal.dylib — no loose .metal/.metallib to ship,
#      4. stages whisper-cli + every dylib FLAT into vendor/whisper/<target>/, rewrites each
#      Mach-O's rpath to @loader_path so it resolves its siblings from its own dir with the
#      environment stripped, and re-signs ad-hoc (arm64 requires a valid signature to exec),
#   5. computes + records the sha256 of every staged file (SHA256SUMS),
#   6. smoke-tests: on native arm64 (and x86_64 via Rosetta if present) runs a real transcription
#      of a generated 3 s WAV and asserts sane JSON output + a Metal init line on stderr.
#
# The result lands in vendor/whisper/<target>/ (gitignored), which WhisperSidecar::locate()
# resolves exe-relative (a whisper/ folder next to the app, or ../Resources/whisper/ inside a
# .app) and which packaging copies next to the app. Record the printed sha256s into
# scripts/whisper-pin.json .targets.<target> and crates/anvil-asr/src/pin.rs WHISPER_PINS.
#
# Usage:
#   scripts/build-whisper-macos.sh                 # both arches, build + stage + smoke
#   scripts/build-whisper-macos.sh --arch arm64    # one arch only (arm64|x86_64|both)
#   scripts/build-whisper-macos.sh --skip-smoke    # build + stage, no transcription smoke test
#   scripts/build-whisper-macos.sh --force         # re-clone / re-build from scratch
#
set -euo pipefail

# --- config / pin ------------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PIN="$SCRIPT_DIR/whisper-pin.json"

# The pin is the source of truth for the tag + deployment target + git url.
GIT_URL="$(jq -r '.targets."macos-aarch64".source_url // "https://github.com/ggml-org/whisper.cpp.git"' "$PIN")"
GIT_REF="$(jq -r '.targets."macos-aarch64".source_ref // "v1.9.1"' "$PIN")"
DEPLOY_TARGET="$(jq -r '.targets."macos-aarch64".deployment_target // "12.0"' "$PIN")"
LICENSE_FILE="$(jq -r '.targets."macos-aarch64".license_file // "whisper.cpp-LICENSE.txt"' "$PIN")"

# Pinned CMake (bootstrapped only if no cmake is on PATH). Official cmake.org universal build.
CMAKE_VER="4.3.3"
CMAKE_SHA256="5221a13450c7a0219a2a0d1b6c9085eb06489721fafd8488ccebc1584175d2fb"

CACHE_DIR="$REPO_ROOT/.cache/whisper"
SRC_DIR="$CACHE_DIR/src"
MODELS_DEV_DIR="$REPO_ROOT/vendor/models-dev"

ARCH_SEL="both"
SKIP_SMOKE=0
FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH_SEL="$2"; shift 2 ;;
    --skip-smoke) SKIP_SMOKE=1; shift ;;
    --force) FORCE=1; shift ;;
    -h|--help) grep '^#' "$0" | sed 's/^#//'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

say_ok()   { printf '  \033[32mok\033[0m  %s\n' "$*"; }
say_step() { printf '\033[36m%s\033[0m\n' "$*"; }
die()      { printf '\033[31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

# --- 1. cmake ----------------------------------------------------------------------------------
ensure_cmake() {
  if command -v cmake >/dev/null 2>&1; then CMAKE="$(command -v cmake)"; return; fi
  local dir="$HOME/.local/cmake-${CMAKE_VER}-macos-universal"
  local cm="$dir/CMake.app/Contents/bin/cmake"
  if [ -x "$cm" ]; then CMAKE="$cm"; say_ok "cmake (bootstrapped) $cm"; return; fi

  say_step "No cmake on PATH — bootstrapping pinned CMake ${CMAKE_VER} into ~/.local (no Homebrew)"
  mkdir -p "$HOME/.local"
  local tgz="$HOME/.local/cmake-${CMAKE_VER}-macos-universal.tar.gz"
  [ -f "$tgz" ] || curl -fsSL -o "$tgz" \
    "https://github.com/Kitware/CMake/releases/download/v${CMAKE_VER}/cmake-${CMAKE_VER}-macos-universal.tar.gz"
  local got; got="$(sha256 "$tgz")"
  [ "$got" = "$CMAKE_SHA256" ] || die "cmake archive sha256 mismatch (expected $CMAKE_SHA256, got $got)"
  say_ok "cmake archive sha256 $got"
  rm -rf "$dir"; tar -xzf "$tgz" -C "$HOME/.local"
  CMAKE="$cm"
  [ -x "$CMAKE" ] || die "cmake bootstrap failed: $CMAKE not executable"
  say_ok "cmake $("$CMAKE" --version | head -1)"
}

# --- 2. source ---------------------------------------------------------------------------------
ensure_src() {
  if [ "$FORCE" = 1 ]; then rm -rf "$SRC_DIR"; fi
  if [ ! -d "$SRC_DIR/.git" ]; then
    say_step "Cloning whisper.cpp $GIT_REF"
    mkdir -p "$CACHE_DIR"
    git clone --depth 1 --branch "$GIT_REF" "$GIT_URL" "$SRC_DIR" 2>&1 | tail -1
  fi
  local desc; desc="$(git -C "$SRC_DIR" describe --tags 2>/dev/null || echo unknown)"
  [ "$desc" = "$GIT_REF" ] || die "source is at $desc, expected $GIT_REF"
  say_ok "whisper.cpp source at $desc"
}

# --- 3+4. build one arch, stage flat, fix rpaths, re-sign --------------------------------------
# $1 = cmake arch (arm64|x86_64), $2 = Cleanroom target (macos-aarch64|macos-x86_64)
build_and_stage() {
  local carch="$1" target="$2"
  local build="$SRC_DIR/build-$carch"
  local dest="$REPO_ROOT/vendor/whisper/$target"

  say_step "Building whisper-cli for $carch ($target), GGML_NATIVE=OFF, deploy $DEPLOY_TARGET"
  [ "$FORCE" = 1 ] && rm -rf "$build"
  "$CMAKE" -S "$SRC_DIR" -B "$build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES="$carch" \
    -DCMAKE_OSX_DEPLOYMENT_TARGET="$DEPLOY_TARGET" \
    -DGGML_NATIVE=OFF \
    -DBUILD_SHARED_LIBS=ON \
    -DWHISPER_BUILD_TESTS=OFF \
    -DWHISPER_BUILD_SERVER=OFF >/dev/null
  "$CMAKE" --build "$build" --target whisper-cli --config Release --parallel >/dev/null
  say_ok "compiled $build/bin/whisper-cli"

  # Stage flat: whisper-cli + every REAL dylib, each under its own install-name basename (the
  # SONAME that @rpath/... references), so the flat set is self-consistent.
  rm -rf "$dest"; mkdir -p "$dest"
  cp "$build/bin/whisper-cli" "$dest/whisper-cli"; chmod +x "$dest/whisper-cli"
  local d soname
  for d in "$build"/bin/*.dylib; do
    [ -L "$d" ] && continue                      # skip the version symlinks
    soname="$(basename "$(otool -D "$d" | tail -1)")"   # @rpath/libX.N.dylib -> libX.N.dylib
    cp "$d" "$dest/$soname"
  done

  # Rewrite every Mach-O's rpath to @loader_path (the build bakes an absolute build-dir rpath,
  # which is neither portable nor something we want to leak into a shipped binary) and re-sign
  # ad-hoc (install_name_tool invalidates the signature; arm64 needs a valid one to exec).
  local f rp
  for f in "$dest"/whisper-cli "$dest"/*.dylib; do
    while rp="$(otool -l "$f" | awk '/LC_RPATH/{getline;getline;print $2; exit}')"; [ -n "${rp:-}" ]; do
      install_name_tool -delete_rpath "$rp" "$f" 2>/dev/null || break
    done
    install_name_tool -add_rpath "@loader_path" "$f"
    codesign --remove-signature "$f" 2>/dev/null || true
    codesign --force --sign - "$f"
  done

  # Ship the MIT licence next to the binaries (redistribution duty).
  cp "$SCRIPT_DIR/licenses/$LICENSE_FILE" "$dest/LICENSE.txt"

  # Assert the staged tree has no dangling non-system, non-@rpath dependency.
  local bad
  bad="$(otool -L "$dest/whisper-cli" | tail -n +2 | awk '{print $1}' \
        | grep -vE '^@rpath/|^/usr/lib/|^/System/' || true)"
  [ -z "$bad" ] || die "whisper-cli has non-portable deps:\n$bad"

  # Assert Metal is embedded (no loose metallib needed at runtime).
  if nm -gU "$dest/libggml-metal.0.dylib" 2>/dev/null | grep -q _ggml_metallib_start; then
    say_ok "Metal library embedded in libggml-metal (no loose .metallib shipped)"
  fi

  # Prove it loads with the environment fully stripped, from its staged location.
  local help; help="$(env -i "$dest/whisper-cli" --help 2>&1 || true)"
  echo "$help" | grep -qiE 'usage|--model' || die "whisper-cli did not print usage in a clean env"
  say_ok "whisper-cli runs env-free from $dest"

  # Record hashes.
  ( cd "$dest" && shasum -a 256 whisper-cli ./*.dylib > SHA256SUMS )
  say_ok "staged $target:"
  ( cd "$dest" && shasum -a 256 whisper-cli ./*.dylib | sed 's/^/     /' )
}

# --- 6. smoke test: real transcription -------------------------------------------------------
locate_model() {
  # An existing whisper model anywhere we can find one, else download tiny.en to vendor/models-dev.
  local m
  for m in \
    "${CLEANROOM_WHISPER_MODEL:-}" \
    "$REPO_ROOT/vendor/models/ggml-tiny.en.bin" \
    "$REPO_ROOT/vendor/models-dev/ggml-tiny.en.bin"; do
    [ -n "$m" ] && [ -f "$m" ] && { echo "$m"; return; }
  done
  # None present: fetch tiny.en and verify the whisper.cpp-published sha1 (same value model.rs pins).
  local url sha1 dst
  url="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin"
  sha1="c78c86eb1a8faa21b369bcd33207cc90d64ae9df"
  dst="$MODELS_DEV_DIR/ggml-tiny.en.bin"
  say_step "No whisper model found — downloading ggml-tiny.en for the smoke test (dev-only)" >&2
  mkdir -p "$MODELS_DEV_DIR"
  curl -fsSL -o "$dst" "$url"
  local got; got="$(shasum -a 1 "$dst" | awk '{print $1}')"
  [ "$got" = "$sha1" ] || die "ggml-tiny.en sha1 mismatch (expected $sha1, got $got)"
  say_ok "ggml-tiny.en sha1 $got (whisper.cpp registry)" >&2
  echo "$dst"
}

make_wav() {
  # A 3 s 16 kHz mono WAV of real speech via macOS `say` (no ffmpeg needed); tone fallback.
  local out="$1"
  if command -v say >/dev/null 2>&1 && \
     say -o "$out" --data-format=LEI16@16000 "Hello, this is a test of the Cleanroom whisper sidecar." 2>/dev/null; then
    return 0
  fi
  python3 - "$out" <<'PY'
import sys, wave, struct, math
w=wave.open(sys.argv[1],'wb'); w.setnchannels(1); w.setsampwidth(2); w.setframerate(16000)
w.writeframes(b''.join(struct.pack('<h',int(3000*math.sin(2*math.pi*220*t/16000))) for t in range(48000)))
w.close()
PY
}

smoke_test() {
  local carch="$1" target="$2"
  local exe="$REPO_ROOT/vendor/whisper/$target/whisper-cli"
  [ -x "$exe" ] || { echo "  (no staged whisper-cli for $target, skipping smoke)"; return; }
  local runner=()
  if [ "$carch" != "arm64" ]; then
    if arch -x86_64 /usr/bin/true 2>/dev/null; then runner=(arch -x86_64)
    else echo "  (no Rosetta — cannot run x86_64 build on this host; staged+hashed only)"; return; fi
  fi

  local model wav base json
  model="$(locate_model)"
  wav="$(mktemp -u).wav"; make_wav "$wav"
  base="$(mktemp -u)"; json="$base.json"
  say_step "Smoke test $target: transcribing a 3 s WAV with $(basename "$model")"
  # NOTE: no -np here. `-np` (no-prints) silences ggml's own log — which is exactly where the
  # Metal init line comes from — so we omit it for the smoke test. The JSON is still written by
  # -oj/-ojf. `${runner[@]+...}` guards the empty-array expansion under `set -u` in bash 3.2.
  local err; err="$("${runner[@]+"${runner[@]}"}" "$exe" -m "$model" -f "$wav" -ml 1 -sow -oj -ojf -of "$base" 2>&1 >/dev/null || true)"
  [ -f "$json" ] || die "whisper-cli produced no JSON ($target)"
  jq -e '.transcription | type == "array"' "$json" >/dev/null || die "whisper JSON has no transcription array ($target)"
  local words; words="$(jq -r '[.transcription[].text] | join(" ")' "$json" | tr -s ' ')"
  say_ok "JSON sane — transcription:$words"
  local metal; metal="$(echo "$err" | grep -iE 'ggml_metal_device_init: GPU name|using embedded metal library' | head -1 | sed 's/^ *//')"
  if [ -n "$metal" ]; then
    say_ok "Metal backend initialised — $metal"
  elif [ "$carch" = arm64 ]; then
    die "native arm64 run showed no Metal init line — Metal accel is required on Apple Silicon"
  else
    printf '  \033[33m!!\033[0m  no Metal init line for %s (x86_64 under Rosetta can differ from native Intel)\n' "$target"
  fi
  rm -f "$wav" "$json"
}

# --- run ---------------------------------------------------------------------------------------
ensure_cmake
ensure_src

do_arch() {  # cmake-arch  target
  build_and_stage "$1" "$2"
  [ "$SKIP_SMOKE" = 1 ] || smoke_test "$1" "$2"
}

case "$ARCH_SEL" in
  arm64)   do_arch arm64  macos-aarch64 ;;
  x86_64)  do_arch x86_64 macos-x86_64 ;;
  both)    do_arch arm64  macos-aarch64; do_arch x86_64 macos-x86_64 ;;
  *) die "unknown --arch $ARCH_SEL (arm64|x86_64|both)" ;;
esac

echo
say_ok "whisper.cpp $GIT_REF macOS sidecars staged under vendor/whisper/"
echo "Record the SHA256SUMS above into scripts/whisper-pin.json (.targets.<target>) and"
echo "crates/anvil-asr/src/pin.rs (WHISPER_PINS). For dev: export CLEANROOM_WHISPER=<staged>/whisper-cli"
