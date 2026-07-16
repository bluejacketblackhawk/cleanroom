#!/usr/bin/env bash
# =============================================================================
# build-ffmpeg-macos.sh — build the LGPL ffmpeg sidecar ANVIL ships on macOS,
# per-arch, from source. The macOS analogue of scripts/fetch-ffmpeg.ps1.
#
# WHY BUILD FROM SOURCE (this is the whole point of this file):
#   ANVIL is MIT and *redistributes* ffmpeg as a sidecar process (never linked).
#   The binary we ship must therefore be GPL-free. On Windows we can pin BtbN's
#   `win64-lgpl` prebuilt. On macOS **no LGPL-clean prebuilt exists**: BtbN
#   publishes no macOS build, and evermeet / osxexperts / martin-riedl /
#   Homebrew all ship GPL builds (x264/x265). So we compile our own, matching
#   the Windows pin discipline: FFmpeg tag n8.1.2, no --enable-gpl / --enable-
#   nonfree, and — deliberately STRICTER than the Windows LGPLv3 pin — no
#   --enable-version3 either, so the result is LGPL-2.1-or-later.
#
#   We build ONE binary PER ARCH (no lipo/universal): Tauri stages the right
#   slice per target, and per-arch keeps the sha256 pin (anvil_media::sidecar)
#   pointing at exactly the bytes that ship for that target.
#
# THIRD-PARTY LIBS (static, LGPL/BSD/MIT/ISC only — audited, sha256-pinned below):
#   libmp3lame 3.100  (LGPL-2.0-or-later)   MP3 encode
#   libopus    1.5.2  (BSD-3-Clause)        Opus encode/decode
#   libogg     1.3.5  (BSD-3-Clause / Xiph) Ogg container
#   libvorbis  1.3.7  (BSD-3-Clause / Xiph) Vorbis encode
#   freetype   2.13.3 (FTL)  font rasterizer for libass. freetype is dual-licensed
#                     FTL / GPLv2; we take the **FTL** side (BSD-style with an
#                     attribution credit) — the GPLv2 side would poison the LGPL bundle.
#   fribidi    1.0.16 (LGPL-2.1-or-later)   Unicode bidi for libass
#   harfbuzz   2.9.1  (Old-MIT)  text shaping — REQUIRED by libass 0.17.x (verified:
#                     libass 0.17.3's configure has no --disable-harfbuzz; its
#                     `harfbuzz >= 1.2.3` pkg-config check is unconditional). 2.9.1 is
#                     the last harfbuzz that ships autotools ./configure — this machine
#                     has no cmake/meson, and 2.9.1 satisfies the >= 1.2.3 requirement.
#   libass     0.17.3 (ISC)  ASS subtitle burn-in. Clip Studio renders EVERY clip
#                     through the `ass=` filter (crates/anvil-media/src/clip.rs), and
#                     the Windows BtbN LGPL build ships libass — parity REQUIREMENT.
#                     Font provider on macOS is CoreText (system framework, autodetected
#                     by libass configure; asserted below). --disable-fontconfig, so no
#                     fontconfig/expat are needed.
#   videotoolbox      (Apple system framework, LGPL-compatible) → h264_videotoolbox,
#                     which crates/anvil-media/src/clip.rs already allowlists.
#   pkgconf    2.3.0  (ISC)  build-only host tool: this Mac has no pkg-config,
#                     and ffmpeg REQUIRES it to detect libopus/libvorbis/libass.
#
# LICENSE TEXTS: every statically-linked third party's notice is extracted from its
# sha256-verified tarball into scripts/licenses/<name>-LICENSE.txt (the repo's canonical
# attribution store, same pattern as fetch-sherpa.ps1's Copy-License) and staged next to
# the binary in vendor/ffmpeg/<target>/ alongside ffmpeg's own LICENSE.txt.
#
# We STRICTLY DO NOT build/enable any GPL or nonfree component (x264, x265,
# xvid, fdk-aac, rubberband, vidstab, frei0r, avisynth, …). After each build the
# binary's `-version` configuration line is scanned against the same forbidden
# marker list the Rust loader and fetch-ffmpeg.ps1 enforce, and it must be empty.
#
# OUTPUT (vendor/ is gitignored; shipped via GH Releases, hash-manifested):
#   vendor/ffmpeg/macos-aarch64/ffmpeg   + LICENSE.txt + configure_line.txt + sha256.txt
#   vendor/ffmpeg/macos-x86_64/ffmpeg    + LICENSE.txt + configure_line.txt + sha256.txt
#
# USAGE:
#   scripts/build-ffmpeg-macos.sh              # build both arches (default)
#   scripts/build-ffmpeg-macos.sh aarch64      # build only arm64
#   scripts/build-ffmpeg-macos.sh x86_64       # build only Intel (cross, via clang -arch)
# =============================================================================
set -euo pipefail

# ---- Pins (versions + sha256 of the exact source tarballs; verified on fetch) ----
FFMPEG_TAG="n8.1.2"
FFMPEG_REPO="https://github.com/FFmpeg/FFmpeg.git"

LAME_VER="3.100";   LAME_SHA="ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e"
LAME_URL="https://downloads.sourceforge.net/project/lame/lame/${LAME_VER}/lame-${LAME_VER}.tar.gz"
OPUS_VER="1.5.2";   OPUS_SHA="65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1"
OPUS_URL="https://downloads.xiph.org/releases/opus/opus-${OPUS_VER}.tar.gz"
OGG_VER="1.3.5";    OGG_SHA="0eb4b4b9420a0f51db142ba3f9c64b333f826532dc0f48c6410ae51f4799b664"
OGG_URL="https://downloads.xiph.org/releases/ogg/libogg-${OGG_VER}.tar.gz"
VORBIS_VER="1.3.7"; VORBIS_SHA="0e982409a9c3fc82ee06e08205b1355e5c6aa4c36bca58146ef399621b0ce5ab"
VORBIS_URL="https://downloads.xiph.org/releases/vorbis/libvorbis-${VORBIS_VER}.tar.gz"
PKGCONF_VER="2.3.0"; PKGCONF_SHA="3a9080ac51d03615e7c1910a0a2a8df08424892b5f13b0628a204d3fcce0ea8b"
PKGCONF_URL="https://distfiles.ariadne.space/pkgconf/pkgconf-${PKGCONF_VER}.tar.xz"
FREETYPE_VER="2.13.3"; FREETYPE_SHA="0550350666d427c74daeb85d5ac7bb353acba5f76956395995311a9c6f063289"
FREETYPE_URL="https://download.savannah.gnu.org/releases/freetype/freetype-${FREETYPE_VER}.tar.xz"
FRIBIDI_VER="1.0.16"; FRIBIDI_SHA="1b1cde5b235d40479e91be2f0e88a309e3214c8ab470ec8a2744d82a5a9ea05c"
FRIBIDI_URL="https://github.com/fribidi/fribidi/releases/download/v${FRIBIDI_VER}/fribidi-${FRIBIDI_VER}.tar.xz"
HARFBUZZ_VER="2.9.1"; HARFBUZZ_SHA="0edcc980f526a338452180e701d6aba6323aef457b6686976a7d17ccbddc51cf"
HARFBUZZ_URL="https://github.com/harfbuzz/harfbuzz/releases/download/${HARFBUZZ_VER}/harfbuzz-${HARFBUZZ_VER}.tar.xz"
LIBASS_VER="0.17.3"; LIBASS_SHA="eae425da50f0015c21f7b3a9c7262a910f0218af469e22e2931462fed3c50959"
LIBASS_URL="https://github.com/libass/libass/releases/download/${LIBASS_VER}/libass-${LIBASS_VER}.tar.xz"

MACOS_MIN="12.0"
export MACOSX_DEPLOYMENT_TARGET="$MACOS_MIN"

# Forbidden configure markers — MUST mirror scripts/ffmpeg-pin.json
# (forbidden_configure_markers) and anvil_media::sidecar::GPL_CONFIGURE_MARKERS.
# Presence of any `--enable-<marker>` in the built binary's configure line is a
# hard build failure: it would mean a GPL/nonfree component slipped in.
FORBIDDEN_MARKERS=(
  gpl nonfree
  avisynth frei0r libcdio libdavs2 libdvdnav libdvdread librubberband
  libvidstab libx264 libx265 libxavs libxavs2 libxvid
  libsmbclient decklink libfdk_aac libmpeghdec
)
# These MUST be present in the configure line — proof we linked what we intended.
REQUIRED_MARKERS=(libmp3lame libopus libvorbis libass videotoolbox)

# ---- Paths ----
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
CACHE="$REPO/.cache/ffmpeg-build"
SRC="$CACHE/src"
BUILD="$CACHE/build"
TOOLS="$CACHE/tools"           # host-only pkgconf lives here
JOBS="$(sysctl -n hw.ncpu)"

mkdir -p "$SRC" "$BUILD" "$TOOLS"

# Progress goes to stderr so a function's stdout carries ONLY its return value
# (build_ffmpeg's stdout is captured with $(...)).
log()  { printf '\033[36m==>\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[32m ok \033[0m %s\n' "$*" >&2; }
warn() { printf '\033[33mwarn\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31mERR\033[0m %s\n' "$*" >&2; exit 1; }

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

fetch_verify() { # url expected_sha out_basename
  local url="$1" sha="$2" out="$SRC/$3"
  if [[ -f "$out" ]] && [[ "$(sha256 "$out")" == "$sha" ]]; then
    ok "cached $(basename "$out")"; return 0
  fi
  log "fetch $url"
  curl -fSL --retry 3 --connect-timeout 30 -o "$out" "$url"
  local got; got="$(sha256 "$out")"
  [[ "$got" == "$sha" ]] || die "sha256 mismatch for $(basename "$out")
  expected: $sha
  actual:   $got"
  ok "verified $(basename "$out") ($got)"
}

extract() { # tarball destdir  -> echoes extracted top dir (on stdout)
  local tb="$SRC/$1" dst="$2"
  rm -rf "$dst"; mkdir -p "$dst"
  tar -xf "$tb" -C "$dst"
  local d
  for d in "$dst"/*/; do
    [[ -d "$d" ]] || continue
    printf '%s\n' "${d%/}"
    return 0
  done
  die "no directory extracted from $1"
}

# libvorbis 1.3.7 / lame 3.100 (and other 2000s-era autotools) inject the PowerPC-era flag
# `-force_cpusubtype_ALL` into CFLAGS on Darwin, which Xcode 26's linker rejects outright
# ("ld: unknown options: -force_cpusubtype_ALL"), failing configure's link probes. Strip it from
# a dep's generated configure before running it. A no-op for deps that don't carry the flag.
depatch() { [[ -f "$1/configure" ]] && sed -i '' 's/-force_cpusubtype_ALL//g' "$1/configure" || true; }

# ---------------------------------------------------------------------------
# 0. Sources
# ---------------------------------------------------------------------------
fetch_all_sources() {
  fetch_verify "$LAME_URL"    "$LAME_SHA"    "lame-${LAME_VER}.tar.gz"
  fetch_verify "$OPUS_URL"    "$OPUS_SHA"    "opus-${OPUS_VER}.tar.gz"
  fetch_verify "$OGG_URL"     "$OGG_SHA"     "libogg-${OGG_VER}.tar.gz"
  fetch_verify "$VORBIS_URL"  "$VORBIS_SHA"  "libvorbis-${VORBIS_VER}.tar.gz"
  fetch_verify "$PKGCONF_URL" "$PKGCONF_SHA" "pkgconf-${PKGCONF_VER}.tar.xz"
  fetch_verify "$FREETYPE_URL" "$FREETYPE_SHA" "freetype-${FREETYPE_VER}.tar.xz"
  fetch_verify "$FRIBIDI_URL"  "$FRIBIDI_SHA"  "fribidi-${FRIBIDI_VER}.tar.xz"
  fetch_verify "$HARFBUZZ_URL" "$HARFBUZZ_SHA" "harfbuzz-${HARFBUZZ_VER}.tar.xz"
  fetch_verify "$LIBASS_URL"   "$LIBASS_SHA"   "libass-${LIBASS_VER}.tar.xz"

  if [[ ! -d "$SRC/ffmpeg/.git" ]]; then
    log "clone FFmpeg $FFMPEG_TAG"
    git clone --depth 1 --branch "$FFMPEG_TAG" "$FFMPEG_REPO" "$SRC/ffmpeg"
  fi
  local desc; desc="$(git -C "$SRC/ffmpeg" describe --tags 2>/dev/null || echo unknown)"
  [[ "$desc" == "$FFMPEG_TAG" ]] || die "FFmpeg checkout is '$desc', expected $FFMPEG_TAG"
  ok "FFmpeg source at $desc"
}

# ---------------------------------------------------------------------------
# 1. pkgconf — build ONCE, native (host tool; same for every target build)
# ---------------------------------------------------------------------------
PKGCONF_BIN="$TOOLS/bin/pkgconf"
build_pkgconf() {
  if [[ -x "$PKGCONF_BIN" ]]; then ok "pkgconf already built"; return; fi
  log "build pkgconf $PKGCONF_VER (host tool)"
  local d; d="$(extract "pkgconf-${PKGCONF_VER}.tar.xz" "$BUILD/pkgconf")"
  ( cd "$d" \
    && ./configure --prefix="$TOOLS" >config.log 2>&1 \
    && make -j"$JOBS" >build.log 2>&1 \
    && make install   >>build.log 2>&1
  ) || die "pkgconf build failed (see $BUILD/pkgconf)"
  [[ -x "$PKGCONF_BIN" ]] || die "pkgconf binary missing after build"
  ok "pkgconf → $PKGCONF_BIN ($("$PKGCONF_BIN" --version))"
}

# ---------------------------------------------------------------------------
# 1b. License texts — extract each linked third party's notice from its verified
#     tarball into scripts/licenses/ (canonical, same pattern the sherpa/whisper
#     lanes use), then stage_licenses_into copies them next to the binary.
#     freetype: docs/FTL.TXT is the FTL side of its FTL/GPLv2 dual — our choice.
# ---------------------------------------------------------------------------
LICENSE_DIR="$SCRIPT_DIR/licenses"
LICENSE_NAMES=(
  libmp3lame-LICENSE.txt opus-LICENSE.txt libogg-LICENSE.txt libvorbis-LICENSE.txt
  freetype-LICENSE.txt fribidi-LICENSE.txt harfbuzz-LICENSE.txt libass-LICENSE.txt
)
extract_licenses() {
  mkdir -p "$LICENSE_DIR"
  local pairs=(
    "lame-${LAME_VER}.tar.gz|lame-${LAME_VER}/COPYING|libmp3lame-LICENSE.txt"
    "opus-${OPUS_VER}.tar.gz|opus-${OPUS_VER}/COPYING|opus-LICENSE.txt"
    "libogg-${OGG_VER}.tar.gz|libogg-${OGG_VER}/COPYING|libogg-LICENSE.txt"
    "libvorbis-${VORBIS_VER}.tar.gz|libvorbis-${VORBIS_VER}/COPYING|libvorbis-LICENSE.txt"
    "freetype-${FREETYPE_VER}.tar.xz|freetype-${FREETYPE_VER}/docs/FTL.TXT|freetype-LICENSE.txt"
    "fribidi-${FRIBIDI_VER}.tar.xz|fribidi-${FRIBIDI_VER}/COPYING|fribidi-LICENSE.txt"
    "harfbuzz-${HARFBUZZ_VER}.tar.xz|harfbuzz-${HARFBUZZ_VER}/COPYING|harfbuzz-LICENSE.txt"
    "libass-${LIBASS_VER}.tar.xz|libass-${LIBASS_VER}/COPYING|libass-LICENSE.txt"
  )
  local p tb member out
  for p in "${pairs[@]}"; do
    IFS='|' read -r tb member out <<<"$p"
    tar -xOf "$SRC/$tb" "$member" > "$LICENSE_DIR/$out" \
      || die "license text $member missing from $tb — a redistributed binary must ship its notice"
    [[ -s "$LICENSE_DIR/$out" ]] || die "extracted empty license $out"
  done
  ok "license texts extracted → scripts/licenses/ (${#pairs[@]} components; freetype = FTL side of dual)"
}
stage_licenses_into() { # destdir
  local dest="$1" n
  for n in "${LICENSE_NAMES[@]}"; do
    [[ -s "$LICENSE_DIR/$n" ]] || die "licence text $LICENSE_DIR/$n missing"
    cp "$LICENSE_DIR/$n" "$dest/$n"
  done
}

# ---------------------------------------------------------------------------
# 2. Static third-party codecs, per-arch
#    arch arg is the clang -arch spelling: arm64 | x86_64
#    (ffmpeg's --arch spelling is aarch64 | x86_64 — resolved in build_ffmpeg)
# ---------------------------------------------------------------------------
deps_prefix() { echo "$BUILD/deps-$1"; }

build_deps() {
  local clang_arch="$1"                       # arm64 | x86_64
  local host                                   # config triple (autotools)
  case "$clang_arch" in
    arm64)  host="aarch64-apple-darwin" ;;
    x86_64) host="x86_64-apple-darwin"  ;;
    *) die "unknown arch $clang_arch" ;;
  esac
  local prefix; prefix="$(deps_prefix "$clang_arch")"
  local cc="clang -arch $clang_arch -mmacosx-version-min=$MACOS_MIN"
  # Explicit --build/--host on EVERY dep so lame 3.100's 2017-vintage config.guess
  # (which predates Apple Silicon) is never consulted; both triples are accepted by
  # its config.sub. This is also what makes the x86_64 pass a clean cross-build.
  local hostflags=(--build="aarch64-apple-darwin" --host="$host")

  mkdir -p "$prefix/lib/pkgconfig"
  log "build static deps for $clang_arch → $prefix (already-built libs are skipped)"

  # PKG_CONFIG_LIBDIR is scoped to ONLY our prefix so nothing on the host leaks in.
  # PKG_CONFIG names our built pkgconf explicitly: this Mac has no pkg-config on PATH,
  # and harfbuzz/libass configure resolve freetype/fribidi/harfbuzz through it.
  export PKG_CONFIG="$PKGCONF_BIN"
  export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
  export PKG_CONFIG_PATH=""

  # zlib is the macOS SDK's (-lz, headers in the SDK) — no .pc file exists in our scoped
  # pkgconfig dir, but freetype's configure resolves zlib via pkg-config. A stub .pc
  # pointing at the system lib keeps the whole dep graph inside PKG_CONFIG_LIBDIR.
  if [[ ! -f "$prefix/lib/pkgconfig/zlib.pc" ]]; then
    printf 'Name: zlib\nDescription: macOS system zlib\nVersion: 1.2.12\nLibs: -lz\nCflags:\n' \
      > "$prefix/lib/pkgconfig/zlib.pc"
  fi

  # -- libogg (vorbis needs it first) --
  if [[ ! -f "$prefix/lib/libogg.a" ]]; then
    local dgg; dgg="$(extract "libogg-${OGG_VER}.tar.gz" "$BUILD/ogg-$clang_arch")"
    depatch "$dgg"
    ( cd "$dgg" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "libogg ($clang_arch) failed — see $BUILD/ogg-$clang_arch"
  fi
  ok "libogg $OGG_VER ($clang_arch)"

  # -- libvorbis (finds ogg via --with-ogg=prefix) --
  if [[ ! -f "$prefix/lib/libvorbis.a" ]]; then
    local dvb; dvb="$(extract "libvorbis-${VORBIS_VER}.tar.gz" "$BUILD/vorbis-$clang_arch")"
    depatch "$dvb"
    ( cd "$dvb" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --with-ogg="$prefix" CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "libvorbis ($clang_arch) failed — see $BUILD/vorbis-$clang_arch"
  fi
  ok "libvorbis $VORBIS_VER ($clang_arch)"

  # -- libopus --
  if [[ ! -f "$prefix/lib/libopus.a" ]]; then
    local dop; dop="$(extract "opus-${OPUS_VER}.tar.gz" "$BUILD/opus-$clang_arch")"
    depatch "$dop"
    ( cd "$dop" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --disable-doc --disable-extra-programs CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "libopus ($clang_arch) failed — see $BUILD/opus-$clang_arch"
  fi
  ok "libopus $OPUS_VER ($clang_arch)"

  # -- libmp3lame (no .pc file; ffmpeg finds it via -I/-L + direct -lmp3lame) --
  if [[ ! -f "$prefix/lib/libmp3lame.a" ]]; then
    local dla; dla="$(extract "lame-${LAME_VER}.tar.gz" "$BUILD/lame-$clang_arch")"
    depatch "$dla"
    ( cd "$dla" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --disable-frontend --disable-decoder CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "libmp3lame ($clang_arch) failed — see $BUILD/lame-$clang_arch"
  fi
  ok "libmp3lame $LAME_VER ($clang_arch)"

  # ---- The libass stack (freetype → fribidi → harfbuzz → libass) ----

  # -- freetype (FTL side of its dual licence; no png/brotli/bzip2/harfbuzz — libass
  #    only needs the rasterizer, and a freetype↔harfbuzz cycle would help nothing) --
  if [[ ! -f "$prefix/lib/libfreetype.a" ]]; then
    local dft; dft="$(extract "freetype-${FREETYPE_VER}.tar.xz" "$BUILD/freetype-$clang_arch")"
    depatch "$dft"
    ( cd "$dft" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --with-zlib=yes --with-bzip2=no --with-png=no --with-harfbuzz=no --with-brotli=no \
           CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "freetype ($clang_arch) failed — see $BUILD/freetype-$clang_arch"
  fi
  ok "freetype $FREETYPE_VER ($clang_arch)"

  # -- fribidi --
  if [[ ! -f "$prefix/lib/libfribidi.a" ]]; then
    local dfb; dfb="$(extract "fribidi-${FRIBIDI_VER}.tar.xz" "$BUILD/fribidi-$clang_arch")"
    depatch "$dfb"
    ( cd "$dfb" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --disable-docs CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "fribidi ($clang_arch) failed — see $BUILD/fribidi-$clang_arch"
  fi
  ok "fribidi $FRIBIDI_VER ($clang_arch)"

  # -- harfbuzz (C++; freetype integration on, every other backend off).
  #    CXXFLAGS: hb.hh promotes its warning list to ERRORS via in-source
  #    `#pragma GCC diagnostic error` (guarded by HB_NO_PRAGMA_GCC_DIAGNOSTIC_ERROR —
  #    upstream's own escape hatch). Xcode 26's clang 21 folds the new
  #    cast-function-type-strict into that group, tripping on hb-ft.cc's classic
  #    FT_Generic_Finalizer casts (harmless FreeType idiom, fixed upstream after 2.9.1).
  #    Define the knob to keep those pragmas as warnings; -Wno-error as second belt. --
  if [[ ! -f "$prefix/lib/libharfbuzz.a" ]]; then
    local dhb; dhb="$(extract "harfbuzz-${HARFBUZZ_VER}.tar.xz" "$BUILD/harfbuzz-$clang_arch")"
    depatch "$dhb"
    ( cd "$dhb" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --with-freetype=yes --with-glib=no --with-cairo=no --with-icu=no \
           --with-graphite2=no --with-coretext=no \
           CC="$cc" CXX="clang++ -arch $clang_arch -mmacosx-version-min=$MACOS_MIN" \
           CXXFLAGS="-g -O2 -DHB_NO_PRAGMA_GCC_DIAGNOSTIC_ERROR -Wno-error=cast-function-type-strict" \
           >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "harfbuzz ($clang_arch) failed — see $BUILD/harfbuzz-$clang_arch"
  fi
  ok "harfbuzz $HARFBUZZ_VER ($clang_arch)"

  # -- libass (CoreText font provider on macOS; NO fontconfig). harfbuzz is a hard
  #    requirement of 0.17.x. x86_64 cross: --disable-asm (its x86 asm needs nasm). --
  if [[ ! -f "$prefix/lib/libass.a" ]]; then
    local asmflag=""
    [[ "$clang_arch" == "x86_64" ]] && asmflag="--disable-asm"
    local das; das="$(extract "libass-${LIBASS_VER}.tar.xz" "$BUILD/libass-$clang_arch")"
    depatch "$das"
    ( cd "$das" \
      && ./configure "${hostflags[@]}" --prefix="$prefix" --disable-shared --enable-static \
           --disable-fontconfig --disable-libunibreak $asmflag \
           CC="$cc" >config.log 2>&1 \
      && make -j"$JOBS" >build.log 2>&1 \
      && make install >>build.log 2>&1
    ) || die "libass ($clang_arch) failed — see $BUILD/libass-$clang_arch"
    # The font provider gate: on macOS libass MUST have compiled in CoreText, or the
    # sidecar would render caption-less clips on end-user machines (no fontconfig here).
    # Mechanical check: the provider's object file is only archived when enabled.
    ar -t "$prefix/lib/libass.a" | grep -q "ass_coretext" \
      || die "libass ($clang_arch) was built WITHOUT the CoreText font provider — check $das/config.log"
  fi
  ok "libass $LIBASS_VER ($clang_arch, CoreText provider)"

  unset PKG_CONFIG PKG_CONFIG_LIBDIR PKG_CONFIG_PATH
}

# ---------------------------------------------------------------------------
# 3. Assert the built binary is a GPL-free LGPL build with the codecs we wanted
# ---------------------------------------------------------------------------
assert_lgpl_and_codecs() { # binary
  local bin="$1"
  local cfg; cfg="$("$bin" -version | grep '^configuration:' || true)"
  [[ -n "$cfg" ]] || die "$bin printed no configuration: line"

  # Mirror the Rust scanner: only --enable-<x> tokens count; normalise - to _.
  local enabled
  enabled="$(printf '%s\n' "$cfg" | tr ' ' '\n' \
              | sed -n 's/^--enable-//p' | tr '-' '_')"
  local m
  for m in "${FORBIDDEN_MARKERS[@]}"; do
    if grep -qx "$m" <<<"$enabled"; then
      die "FORBIDDEN marker enabled in $bin: --enable-$m (GPL/nonfree — refusing)"
    fi
  done
  # Belt-and-suspenders: the switches themselves must be literally absent too.
  grep -q -- '--enable-gpl'      <<<"$cfg" && die "$bin has --enable-gpl"
  grep -q -- '--enable-nonfree'  <<<"$cfg" && die "$bin has --enable-nonfree"
  grep -q -- '--enable-version3' <<<"$cfg" && die "$bin has --enable-version3 (want LGPL-2.1, not v3)"
  for m in "${REQUIRED_MARKERS[@]}"; do
    grep -qx "$m" <<<"$enabled" || die "$bin is MISSING required --enable-$m"
  done
  ok "LGPL gate passed: no GPL/nonfree/version3; has ${REQUIRED_MARKERS[*]}"
}

# ---------------------------------------------------------------------------
# 4. ffmpeg, per-arch
# ---------------------------------------------------------------------------
configure_ffmpeg() { # build_dir clang_arch ff_arch prefix cross extra_ld
  local bdir="$1" clang_arch="$2" ff_arch="$3" deps="$4" cross="$5" extra_ld="$6"
  local cc="clang -arch $clang_arch -mmacosx-version-min=$MACOS_MIN"
  local args=(
    --prefix="$bdir/out"
    --arch="$ff_arch"
    --target-os=darwin
    --cc="$cc"
    --host-cc=clang
    --pkg-config="$PKGCONF_BIN"
    --pkg-config-flags=--static
    --extra-cflags="-I$deps/include"
    --extra-ldflags="-L$deps/lib $extra_ld"
    # Static libharfbuzz is C++ (needs the C++ runtime at final link), and static
    # libass' CoreText font provider needs the CoreText/CoreFoundation/CoreGraphics
    # system frameworks. Same pattern as BtbN's `--extra-libs=-lgomp`.
    --extra-libs="-lc++ -framework CoreText -framework CoreFoundation -framework CoreGraphics"
    --disable-shared
    --enable-static
    --disable-autodetect
    --enable-pthreads
    --enable-zlib
    --disable-debug
    --disable-doc
    --disable-ffplay
    --disable-ffprobe
    --enable-libmp3lame
    --enable-libopus
    --enable-libvorbis
    --enable-libass
    --enable-videotoolbox
  )
  # ffprobe is NOT invoked anywhere in the workspace (verified: anvil-media parses
  # `ffmpeg -i` stderr banners + `-progress` k/v lines only), so we drop it.
  if [[ "$cross" == "1" ]]; then
    # x86_64 from an arm64 host: cross-compile, and no nasm/yasm on this box, so no
    # hand-written x86 asm (C fallbacks only — correctness identical, slightly slower).
    args+=(--enable-cross-compile --disable-x86asm)
  fi
  ( cd "$bdir" && "$SRC/ffmpeg/configure" "${args[@]}" ) >"$bdir/configure.log" 2>&1
}

build_ffmpeg() {
  local clang_arch="$1"                       # arm64 | x86_64
  local ff_arch cross target_key
  case "$clang_arch" in
    arm64)  ff_arch="aarch64"; cross="0"; target_key="macos-aarch64" ;;
    x86_64) ff_arch="x86_64";  cross="1"; target_key="macos-x86_64"  ;;
    *) die "unknown arch $clang_arch" ;;
  esac
  local deps; deps="$(deps_prefix "$clang_arch")"
  local bdir="$BUILD/ffmpeg-$clang_arch"
  rm -rf "$bdir"; mkdir -p "$bdir"

  # ffmpeg's configure shells out to pkgconf to find libopus/libvorbis; scope it to
  # ONLY our per-arch prefix so the host can't leak a stray .pc in. (build_deps unsets
  # these on exit / when cached, so we must set them here for the ffmpeg pass.)
  export PKG_CONFIG_LIBDIR="$deps/lib/pkgconfig"
  export PKG_CONFIG_PATH=""

  log "configure+build ffmpeg for $clang_arch (ff-arch=$ff_arch, cross=$cross)"
  if ! ( configure_ffmpeg "$bdir" "$clang_arch" "$ff_arch" "$deps" "$cross" "" \
         && cd "$bdir" && make -j"$JOBS" >make.log 2>&1 && [[ -f ffmpeg ]] ); then
    # Xcode 15+ new linker occasionally chokes on autotools static links; retry once
    # with the classic linker (a known gotcha). If -ld_classic itself is gone in this
    # Xcode, this second attempt fails and we report honestly.
    log "primary link failed — retrying $clang_arch with -Wl,-ld_classic"
    rm -rf "$bdir"; mkdir -p "$bdir"
    ( configure_ffmpeg "$bdir" "$clang_arch" "$ff_arch" "$deps" "$cross" "-Wl,-ld_classic" \
      && cd "$bdir" && make -j"$JOBS" >make.log 2>&1 && [[ -f ffmpeg ]] ) \
      || die "ffmpeg build for $clang_arch failed (see $bdir/configure.log, $bdir/make.log)"
    local used_ld_classic=1
  fi
  local bin="$bdir/ffmpeg"
  ok "built $bin"

  # Ad-hoc codesign BEFORE hashing/running: arm64 Mach-O won't exec without a
  # signature (kernel requirement); sign x86_64 too for consistency. Signing
  # rewrites the binary, so the sha256 we pin must be taken AFTER this.
  codesign --force --sign - --timestamp=none "$bin"
  ok "ad-hoc codesigned $clang_arch"

  # Stage into vendor/ (gitignored) + license text + provenance sidecar files.
  local stage="$REPO/vendor/ffmpeg/$target_key"
  rm -rf "$stage"; mkdir -p "$stage"
  cp "$bin" "$stage/ffmpeg"
  cp "$SRC/ffmpeg/COPYING.LGPLv2.1" "$stage/LICENSE.txt"
  stage_licenses_into "$stage"
  "$stage/ffmpeg" -version | grep '^configuration:' | sed "s|$REPO/|\$REPO/|g" > "$stage/configure_line.txt"
  sha256 "$stage/ffmpeg" > "$stage/sha256.txt"
  [[ "${used_ld_classic:-0}" == "1" ]] && echo "$clang_arch required -Wl,-ld_classic" > "$stage/USED_LD_CLASSIC.txt"

  echo "$target_key"                            # return the staged target key (stdout)
}

# ---------------------------------------------------------------------------
# 5. Smoke test — encode a 1 s sine to mp3 + opus + flac (+ vorbis), decode back,
#    and burn an ASS caption through the libass filter (proves libass + CoreText
#    font lookup actually render, not merely link).
# ---------------------------------------------------------------------------
smoke() { # binary runner...(prefix to exec, e.g. `arch -x86_64`)
  local bin="$1"; shift
  local run=("$@")
  local tmp; tmp="$(mktemp -d)"
  local rc=0
  {
    "${run[@]}" "$bin" -hide_banner -loglevel error -y \
      -f lavfi -i "sine=frequency=440:duration=1" "$tmp/a.mp3"  &&
    "${run[@]}" "$bin" -hide_banner -loglevel error -y \
      -f lavfi -i "sine=frequency=440:duration=1" -c:a libopus "$tmp/a.opus" &&
    "${run[@]}" "$bin" -hide_banner -loglevel error -y \
      -f lavfi -i "sine=frequency=440:duration=1" "$tmp/a.flac" &&
    "${run[@]}" "$bin" -hide_banner -loglevel error -y \
      -f lavfi -i "sine=frequency=440:duration=1" -c:a libvorbis "$tmp/a.ogg" &&
    for f in a.mp3 a.opus a.flac a.ogg; do
      "${run[@]}" "$bin" -hide_banner -loglevel error -y -i "$tmp/$f" \
        -f f32le "$tmp/${f}.pcm" || { rc=1; break; }
      [[ -s "$tmp/${f}.pcm" ]] || { rc=1; break; }
    done
  } || rc=1

  # libass burn-in: one bright caption on a black canvas; the mean luma of the
  # rendered gray frame must clearly exceed black, so a font-provider failure
  # (caption silently not drawn) fails the smoke rather than passing vacuously.
  cat > "$tmp/smoke.ass" <<'ASS'
[Script Info]
ScriptType: v4.00+
PlayResX: 320
PlayResY: 240
[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,96,&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,2,0,5,10,10,10,1
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,SMOKE
ASS
  local luma=""
  if ( cd "$tmp" && "${run[@]}" "$bin" -hide_banner -loglevel error -y \
         -f lavfi -i "color=c=black:s=320x240:r=10:d=0.5" \
         -vf "ass=filename=smoke.ass" -frames:v 1 \
         -f rawvideo -pix_fmt gray gray.bin ) && [[ -s "$tmp/gray.bin" ]]; then
    # mean pixel value of the gray frame, integer.
    luma="$(python3 - "$tmp/gray.bin" <<'PY'
import sys
d = open(sys.argv[1], "rb").read()
print(sum(d) // max(len(d), 1))
PY
)"
    [[ -n "$luma" && "$luma" -gt 2 ]] || rc=1
  else
    rc=1
  fi

  local n; n="$(ls -1 "$tmp"/*.pcm 2>/dev/null | wc -l | tr -d ' ')"
  local label="${run[*]}"
  [[ "$label" == "env" ]] && label="native"
  if [[ "$rc" == "0" ]]; then
    ok "smoke ($label): mp3/opus/flac/vorbis round-trip OK ($n pcm outs); libass burn-in lit the frame (mean luma $luma > 2)"
  else
    warn "smoke ($label): FAILED (pcm outs: $n, caption luma: ${luma:-none})"
  fi
  rm -rf "$tmp"
  return $rc
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
ARCHES=("$@")
[[ ${#ARCHES[@]} -eq 0 ]] && ARCHES=(arm64 x86_64)

log "ANVIL macOS ffmpeg build — arches: ${ARCHES[*]} — jobs: $JOBS"
[[ "$(uname -m)" == "arm64" ]] || die "this script assumes an arm64 host (got $(uname -m))"

fetch_all_sources
build_pkgconf
extract_licenses

declare -a SUMMARY=()
for a in "${ARCHES[@]}"; do
  build_deps "$a"
  key="$(build_ffmpeg "$a")"
  bin="$REPO/vendor/ffmpeg/$key/ffmpeg"
  assert_lgpl_and_codecs "$bin"

  if [[ "$a" == "arm64" ]]; then
    # `env` is a no-op runner prefix: it keeps the runner array non-empty, which macOS' bash 3.2
    # requires to expand "${run[@]}" under `set -u` (an empty array there is an "unbound variable").
    smoke "$bin" env || true                     # native
  else
    # x86_64 exec depends on Rosetta 2. Try it; if absent, we still ship the hash.
    if arch -x86_64 /usr/bin/true 2>/dev/null; then
      smoke "$bin" arch -x86_64 || true
    else
      warn "Rosetta 2 not installed — x86_64 binary staged+hashed but not smoke-run here (S7 QA validates on Intel)."
    fi
  fi

  if otool -L "$bin" | tail -n +2 | grep -qvE '/usr/lib/|/System/Library/'; then
    warn "$key links a non-system dylib (unexpected for a static build):"
    otool -L "$bin" | tail -n +2 | grep -Ev '/usr/lib/|/System/Library/' >&2 || true
  else
    ok "$key links only system libs/frameworks (self-contained)"
  fi

  h="$(cat "$REPO/vendor/ffmpeg/$key/sha256.txt")"
  ver="$("$bin" -version | head -1 | awk '{print $3}')"
  bytes="$(stat -f%z "$bin")"
  SUMMARY+=("$key|$ver|$bytes|$h")
done

echo
echo "============================== BUILD SUMMARY =============================="
printf 'FFmpeg tag: %s   deps: lame %s / opus %s / ogg %s / vorbis %s / freetype %s (FTL) / fribidi %s / harfbuzz %s / libass %s (CoreText)\n' \
  "$FFMPEG_TAG" "$LAME_VER" "$OPUS_VER" "$OGG_VER" "$VORBIS_VER" \
  "$FREETYPE_VER" "$FRIBIDI_VER" "$HARFBUZZ_VER" "$LIBASS_VER"
for row in "${SUMMARY[@]}"; do
  IFS='|' read -r key ver bytes h <<<"$row"
  echo
  echo "target       : $key"
  echo "version      : $ver"
  echo "binary_bytes : $bytes"
  echo "binary_sha256: $h"
  echo "configure    : $(cat "$REPO/vendor/ffmpeg/$key/configure_line.txt")"
  [[ -f "$BUILD/ffmpeg-${key#macos-}/USED_LD_CLASSIC" ]] && echo "ld_classic   : YES"
done
echo "=========================================================================="
echo "Staged under vendor/ffmpeg/<target>/  (gitignored). Point ANVIL at one with:"
echo "  export ANVIL_FFMPEG=$REPO/vendor/ffmpeg/macos-aarch64/ffmpeg"
