//! ffmpeg sidecar manager.
//!
//! ffmpeg is **never linked** into ANVIL — it is run as a separate child process. This
//! keeps the engine MIT-clean: we only ship / invoke an unmodified **LGPL** ffmpeg build,
//! at arm's length, for the containers symphonia can't demux (mkv/webm/mov and other video
//! wrappers) and any codec symphonia lacks.
//!
//! Airplane-mode guarantees (ADR-005, engine invariant): this module **never downloads**
//! anything. The binary must already be present — bundled next to the app, pointed at by
//! `ANVIL_FFMPEG`, or on `PATH`. Before running it we verify its sha256 against a pinned
//! constant so a swapped/hostile binary can't be executed silently.
//!
//! Decoding pipes raw `f32le` PCM out of ffmpeg on stdout and reads `-progress` key/value
//! lines to compute a completion fraction (see [`FfmpegSidecar::decode_to_buffer`]).
//!
//! ## Integrity policy (this is a licence control, not just a checksum)
//! The binary we execute is hashed and compared to this platform's [`FFMPEG_PINS`] entry **before
//! every use**. The pin identifies one exact, audited, GPL-free build (see [`FFMPEG_PINS`] and
//! `scripts/fetch-ffmpeg.ps1` / `scripts/build-ffmpeg-macos.sh`).
//!
//! On **macOS** the compared hash is the signing-independent Mach-O **content hash**
//! ([`macho_content_sha256`]), not the raw file sha256: every bundled sidecar is Developer-ID
//! re-signed at packaging time, which rewrites the raw Mach-O bytes (the code-signature blob and
//! the header fields describing its size) but not the program itself. Pinning the content keeps
//! the gate stable across ad-hoc/Developer-ID/re-signing while still refusing any real tamper.
//! On **Windows/other** the gate stays the raw [`sha256_file`] (Authenticode gets the same
//! treatment when Windows signing lands — ADR-012). Where a candidate came from decides what a
//! mismatch means:
//!
//! | Source | On mismatch |
//! |---|---|
//! | bundled sidecar next to the exe (**this is what ships**) | hard [`MediaError::SidecarHashMismatch`] — nothing can bypass it |
//! | `ffmpeg` found on `PATH` | hard [`MediaError::SidecarHashMismatch`] |
//! | `ANVIL_FFMPEG=<path>` (a developer typed this) | hard error **unless** `ANVIL_FFMPEG_ALLOW_UNPINNED=1` is *also* set, which downgrades it to a loud warning |
//!
//! So ANVIL runs an unverified ffmpeg only when a human both pointed it at one *and* said
//! out loud that unverified is acceptable — two deliberate acts, neither of which a shipped
//! build performs. That keeps dev ergonomics (the repo's dev ffmpeg is a **GPL** gyan build,
//! which must never ship) without leaving the shipped sidecar bypassable.
//!
//! Each vendored platform has its own pin in [`FFMPEG_PINS`] (Windows: BtbN's LGPLv3 prebuilt;
//! macOS arm64 + x86_64: our own LGPL-2.1 builds from `scripts/build-ffmpeg-macos.sh`). A platform
//! with no entry yet (e.g. Linux — M6) has **no pin**, so [`pinned_sha256`] returns `None` and
//! every non-developer candidate is refused with a [`MediaError::SidecarFailed`] naming the missing
//! platform. Refusing is deliberate: "we have not vendored an LGPL build for this OS" must fail
//! loudly at the seam, not ship an unaudited binary.
//!
//! ## Proving the build is LGPL
//! [`FfmpegSidecar::assert_lgpl`] runs `-version` and scans the `configuration:` line for
//! [`GPL_CONFIGURE_MARKERS`]. This is not a heuristic: FFmpeg's own `configure` calls
//! `die_license_disabled gpl` over `EXTERNAL_LIBRARY_GPL_LIST`, so a build that links x264,
//! x265, rubberband, vidstab, frei0r, … *cannot* be produced without `--enable-gpl` appearing
//! in that line. Absence of every marker is therefore a mechanical proof of a GPL-free build,
//! and the shipped binary is checked against it in `tests/sidecar_pin.rs`.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use anvil_core::BLOCK_SAMPLES;
use sha2::{Digest, Sha256};

use crate::error::MediaError;
use crate::probe::MediaInfo;
use crate::AudioBuffer;

/// Internal decode rate all sidecar output is resampled to by ffmpeg's `-ar`.
const OUT_SAMPLE_RATE: u32 = anvil_core::INTERNAL_SAMPLE_RATE;

/// One vendored ffmpeg build ANVIL ships, recorded in code so a licence audit never has to guess
/// what the shipped bytes are. There is one entry per platform target in [`FFMPEG_PINS`];
/// `scripts/ffmpeg-pin.json` carries the same values for the provisioning scripts, and
/// `pin_json_matches_the_code` keeps the two honest.
///
/// **Windows** is BtbN's prebuilt `win64-lgpl` build of the FFmpeg 8.1 branch — an immutable
/// release asset, so it has `archive_*` provenance. It sets `--enable-version3` (for gmp /
/// libaribb24 / libopencore-amr), which FFmpeg's configure resolves to **LGPL v3**. Still an
/// arm's-length licence for a separately-distributed desktop app, but LGPLv3 carries GPLv3 terms
/// that are incompatible with the Apple App Store; ANVIL ships from GitHub Releases, so it does
/// not bite here.
///
/// **macOS** has no LGPL-clean prebuilt anywhere (BtbN publishes no macOS build; evermeet,
/// osxexperts, martin-riedl and Homebrew are all GPL), so we **build it from source**, per-arch,
/// with `scripts/build-ffmpeg-macos.sh` — no `--enable-gpl`, no `--enable-nonfree`, and
/// deliberately no `--enable-version3` either, so the mac builds are **LGPL-2.1-or-later**:
/// stricter than the Windows pin. A built-from-source entry has no archive, so
/// [`FfmpegPin::archive_sha256`] / [`FfmpegPin::archive_member`] are `None` and
/// [`FfmpegPin::source_url`] carries the build-script provenance instead of a URL.
///
/// ffmpeg is used **only as a child process** — never linked — so its licence imposes nothing on
/// ANVIL's MIT licence. Redistribution duties (handoff/07 §2) are met by shipping the vendored
/// tree: `LICENSE.txt` next to the binary, the exact configure line discoverable (`ffmpeg
/// -version`, mirrored in [`FfmpegPin::configure_line`]), and [`FfmpegPin::source_url`] as the
/// permanent source pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfmpegPin {
    /// Upstream FFmpeg version string, as `ffmpeg -version` reports it.
    pub version: &'static str,
    /// `<os>-<arch>` this build is for, matching `format!("{}-{}", OS, ARCH)` over
    /// [`std::env::consts`].
    pub target: &'static str,
    /// For a **prebuilt** target, the immutable release-asset URL the archive is fetched from.
    /// For a **built-from-source** target (macOS), a provenance string naming the build script.
    /// Either way, the permanent source pointer.
    pub source_url: &'static str,
    /// sha256 of the downloaded archive (lowercase hex), or `None` for built-from-source targets.
    pub archive_sha256: Option<&'static str>,
    /// Path of the binary inside the archive, or `None` for built-from-source targets.
    pub archive_member: Option<&'static str>,
    /// Raw sha256 of the extracted/built binary. The **provision-time** hash: `fetch-ffmpeg.ps1`
    /// / `build-ffmpeg-macos.sh` / `stage-mac-sidecars.sh` verify the vendored bytes against it,
    /// and it is the hash [`verify_hash`] enforces on non-macOS targets.
    pub binary_sha256: &'static str,
    /// Signing-independent Mach-O **content** hash — sha256 of the binary with its code-signature
    /// blob and the three signature-size header fields it perturbs neutralised (see
    /// [`macho_content_sha256`]). `Some` for every macOS target (where a Developer-ID re-sign
    /// rewrites the raw bytes but not the content), `None` for Windows/PE, which stays raw
    /// ([`binary_sha256`](Self::binary_sha256)) until Windows signing lands. This is the hash
    /// [`verify_hash`] enforces at run time on macOS.
    pub content_sha256: Option<&'static str>,
    /// SPDX licence of the build as a whole.
    pub license: &'static str,
    /// Filename of the licence text shipped next to the binary.
    pub license_file: &'static str,
    /// The exact `configuration:` line `ffmpeg -version` prints for this build — its own record
    /// of what it was compiled against, scanned by [`gpl_markers_in`].
    pub configure_line: &'static str,
}

/// The vendored LGPL ffmpeg builds, one per platform target. [`current_pin`] / [`pinned_sha256`]
/// select the entry for `format!("{}-{}", OS, ARCH)`; a platform with no entry has **no pin** and
/// every non-developer candidate is refused (see the module docs). This table is the schema's
/// source of truth in code; `pin_json_matches_the_code` asserts it never drifts from
/// `scripts/ffmpeg-pin.json`.
pub static FFMPEG_PINS: &[FfmpegPin] = &[
    // Windows — BtbN win64-lgpl prebuilt (LGPL v3, via --enable-version3). Content preserved
    // byte-for-byte from the original single-target pin.
    FfmpegPin {
        version: "n8.1.2-22-g94138f6973",
        target: "windows-x86_64",
        source_url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-14-13-19/ffmpeg-n8.1.2-22-g94138f6973-win64-lgpl-8.1.zip",
        archive_sha256: Some("98e6b1d20e083ab34fd275509682fd12b7ea4bd205a7814331704d6feba9a0c4"),
        archive_member: Some("ffmpeg-n8.1.2-22-g94138f6973-win64-lgpl-8.1/bin/ffmpeg.exe"),
        binary_sha256: "86f25f2d5487b84ceb994d022a8c2b9424b7042d8c1c0c209a597f561e891392",
        // Windows/PE: the runtime gate stays on the raw hash (Authenticode gets content-hashed
        // when Windows signing lands — see the module docs and ADR-012).
        content_sha256: None,
        license: "LGPL-3.0-or-later",
        license_file: "ffmpeg-n8.1.2-22-g94138f6973-win64-lgpl-8.1/LICENSE.txt",
        configure_line: WINDOWS_CONFIGURE_LINE,
    },
    // macOS arm64 — built from source (LGPL-2.1-or-later); values from build-ffmpeg-macos.sh.
    FfmpegPin {
        version: "n8.1.2",
        target: "macos-aarch64",
        source_url: "built-from-source (scripts/build-ffmpeg-macos.sh)",
        archive_sha256: None,
        archive_member: None,
        binary_sha256: MACOS_AARCH64_SHA256,
        content_sha256: Some(MACOS_AARCH64_CONTENT_SHA256),
        license: "LGPL-2.1-or-later",
        license_file: "LICENSE.txt",
        configure_line: MACOS_AARCH64_CONFIGURE_LINE,
    },
    // macOS x86_64 — built from source (LGPL-2.1-or-later); values from build-ffmpeg-macos.sh.
    FfmpegPin {
        version: "n8.1.2",
        target: "macos-x86_64",
        source_url: "built-from-source (scripts/build-ffmpeg-macos.sh)",
        archive_sha256: None,
        archive_member: None,
        binary_sha256: MACOS_X86_64_SHA256,
        content_sha256: Some(MACOS_X86_64_CONTENT_SHA256),
        license: "LGPL-2.1-or-later",
        license_file: "LICENSE.txt",
        configure_line: MACOS_X86_64_CONFIGURE_LINE,
    },
];

// --- Per-build values captured by scripts/build-ffmpeg-macos.sh on this machine. -------------
// (Filled from vendor/ffmpeg/macos-*/sha256.txt and configure_line.txt after the build.)
const MACOS_AARCH64_SHA256: &str =
    "350a70538452110ec836ba1af99ab7691a1cfc01ef96a87f91a1baef1fc9cd7e";
const MACOS_X86_64_SHA256: &str =
    "9d91f6d1a695615b26003ebb54192fe151e1ff75b17fbb5fda78f50c9e67421c";
// Signing-independent content hashes (see [`macho_content_sha256`]) of the SAME builds. Computed
// from the ad-hoc-signed vendor binary; identical for the Developer-ID-signed copy that ships in
// the `.app` — which is exactly the invariant the runtime gate now depends on (a signed sidecar
// must still pass). Recorded here and in `scripts/ffmpeg-pin.json`, kept honest by
// `pin_json_matches_the_code`, and proven equal to the shipped `.app` copy by
// `tests/sidecar_pin.rs::signed_bundle_ffmpeg_content_hash_matches_the_pin`.
const MACOS_AARCH64_CONTENT_SHA256: &str =
    "eb705d29d54661c3174483e1d2ad02df33a4a38bea3f437895958e7abf4a39de";
const MACOS_X86_64_CONTENT_SHA256: &str =
    "105dee680ec95f06ef79f2c2ea8fad1e38dec844d6b8401fea1efec4e1b01dab";
const MACOS_AARCH64_CONFIGURE_LINE: &str = "configuration: --prefix=$REPO/.cache/ffmpeg-build/build/ffmpeg-arm64/out --arch=aarch64 --target-os=darwin --cc='clang -arch arm64 -mmacosx-version-min=12.0' --host-cc=clang --pkg-config=$REPO/.cache/ffmpeg-build/tools/bin/pkgconf --pkg-config-flags=--static --extra-cflags=-I$REPO/.cache/ffmpeg-build/build/deps-arm64/include --extra-ldflags='-L$REPO/.cache/ffmpeg-build/build/deps-arm64/lib ' --extra-libs='-lc++ -framework CoreText -framework CoreFoundation -framework CoreGraphics' --disable-shared --enable-static --disable-autodetect --enable-pthreads --enable-zlib --disable-debug --disable-doc --disable-ffplay --disable-ffprobe --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libass --enable-videotoolbox";
const MACOS_X86_64_CONFIGURE_LINE: &str = "configuration: --prefix=$REPO/.cache/ffmpeg-build/build/ffmpeg-x86_64/out --arch=x86_64 --target-os=darwin --cc='clang -arch x86_64 -mmacosx-version-min=12.0' --host-cc=clang --pkg-config=$REPO/.cache/ffmpeg-build/tools/bin/pkgconf --pkg-config-flags=--static --extra-cflags=-I$REPO/.cache/ffmpeg-build/build/deps-x86_64/include --extra-ldflags='-L$REPO/.cache/ffmpeg-build/build/deps-x86_64/lib ' --extra-libs='-lc++ -framework CoreText -framework CoreFoundation -framework CoreGraphics' --disable-shared --enable-static --disable-autodetect --enable-pthreads --enable-zlib --disable-debug --disable-doc --disable-ffplay --disable-ffprobe --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libass --enable-videotoolbox --enable-cross-compile --disable-x86asm";

/// The `configuration:` line the Windows BtbN build reports (captured when vendored). A module
/// const so the GPL scanner is tested against the real shipped bytes even where the binary is not
/// provisioned. Mirrored verbatim in `scripts/ffmpeg-pin.json` (`windows-x86_64.configure_line`).
const WINDOWS_CONFIGURE_LINE: &str = "configuration: --prefix=/ffbuild/prefix --pkg-config-flags=--static --pkg-config=pkg-config --cross-prefix=x86_64-w64-mingw32- --arch=x86_64 --target-os=mingw32 --enable-version3 --disable-debug --disable-w32threads --enable-pthreads --enable-iconv --enable-zlib --enable-libxml2 --enable-libvmaf --enable-fontconfig --enable-libharfbuzz --enable-libfreetype --enable-libfribidi --enable-vulkan --enable-libshaderc --enable-libvorbis --disable-libxcb --disable-xlib --disable-libpulse --enable-gmp --enable-lzma --enable-liblcevc-dec --enable-opencl --enable-amf --enable-libaom --enable-libaribb24 --disable-avisynth --enable-chromaprint --enable-libdav1d --disable-libdavs2 --disable-libdvdread --disable-libdvdnav --disable-libfdk-aac --enable-ffnvcodec --enable-cuda-llvm --disable-frei0r --enable-libgme --enable-libkvazaar --enable-libaribcaption --enable-libass --enable-libbluray --enable-libjxl --enable-libmp3lame --enable-libopus --enable-libplacebo --enable-librist --enable-libssh --enable-libtheora --enable-libvpx --enable-libwebp --enable-libzmq --enable-lv2 --enable-libvpl --enable-openal --enable-liboapv --enable-libopencore-amrnb --enable-libopencore-amrwb --enable-libopenh264 --enable-libopenjpeg --enable-libopenmpt --enable-librav1e --disable-librubberband --enable-schannel --enable-sdl2 --enable-libsnappy --enable-libsoxr --enable-libsrt --enable-libsvtav1 --enable-libtwolame --enable-libuavs3d --disable-libdrm --enable-vaapi --disable-libvidstab --enable-libvvenc --disable-whisper --disable-libx264 --disable-libx265 --disable-libxavs2 --disable-libxvid --enable-libzimg --enable-libzvbi --extra-cflags=-DLIBTWOLAME_STATIC --extra-libs=-lgomp --extra-ldflags=-pthread --extra-version=20260714";

/// Set to `1`/`true` to let an `ANVIL_FFMPEG`-supplied binary run without matching the pin.
/// Developer-only (the repo's dev ffmpeg is a GPL build); never set in a shipped app.
pub const ALLOW_UNPINNED_ENV: &str = "ANVIL_FFMPEG_ALLOW_UNPINNED";

/// Configure-line tokens that mean a binary is **not** redistributable under ANVIL's terms:
/// the GPL-only external libraries (FFmpeg's `EXTERNAL_LIBRARY_GPL_LIST` +
/// `EXTERNAL_LIBRARY_GPLV3_LIST`), the nonfree ones (`EXTERNAL_LIBRARY_NONFREE_LIST`), and the
/// `gpl`/`nonfree` switches themselves. Names are in configure's underscore spelling; the
/// scanner normalises `-` to `_` before comparing, so `--enable-libfdk-aac` and
/// `--enable-libfdk_aac` both trip.
///
/// Note `version3` is deliberately absent: it means LGPL**v3**, which is not GPL. See
/// [`FFMPEG_PINS`].
pub const GPL_CONFIGURE_MARKERS: &[&str] = &[
    // the switches themselves
    "gpl",
    "nonfree",
    // EXTERNAL_LIBRARY_GPL_LIST (ffmpeg release/8.1 configure)
    "avisynth",
    "frei0r",
    "libcdio",
    "libdavs2",
    "libdvdnav",
    "libdvdread",
    "librubberband",
    "libvidstab",
    "libx264",
    "libx265",
    "libxavs",
    "libxavs2",
    "libxvid",
    // EXTERNAL_LIBRARY_GPLV3_LIST
    "libsmbclient",
    // EXTERNAL_LIBRARY_NONFREE_LIST
    "decklink",
    "libfdk_aac",
    "libmpeghdec",
];

/// The [`FfmpegPin`] for the platform we are running on, or `None` if no LGPL build has been
/// vendored for it yet (e.g. Linux — M6). The runtime target is `format!("{}-{}", OS, ARCH)`,
/// e.g. `"macos-aarch64"`, `"windows-x86_64"`.
pub fn current_pin() -> Option<&'static FfmpegPin> {
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    FFMPEG_PINS.iter().find(|p| p.target == target)
}

/// The pinned **raw** sha256 for the platform we are running on, or `None` if no LGPL build has
/// been vendored for it yet. `None` means every bundled/`PATH` candidate is refused: see the
/// module docs. This is the provision-time hash; on macOS the *runtime* gate enforces
/// [`content_pinned_sha256`] instead (a Developer-ID re-sign changes the raw bytes).
pub fn pinned_sha256() -> Option<&'static str> {
    current_pin().map(|p| p.binary_sha256)
}

/// The pinned signing-independent **content** hash for the platform we are running on, or `None`
/// on a platform whose pin carries none (Windows/PE, or an unvendored OS). On macOS this is the
/// hash [`verify_hash`] actually enforces at run time — see [`macho_content_sha256`].
pub fn content_pinned_sha256() -> Option<&'static str> {
    current_pin().and_then(|p| p.content_sha256)
}

/// Every [`GPL_CONFIGURE_MARKERS`] entry that a configure line actually **enables**. Empty means
/// the build is GPL-free. Only `--enable-` tokens are considered, so the `--disable-libx264` an
/// LGPL build carries is (correctly) not a hit.
pub fn gpl_markers_in(configure_line: &str) -> Vec<&'static str> {
    let enabled: Vec<String> = configure_line
        .split_whitespace()
        .filter_map(|tok| tok.strip_prefix("--enable-"))
        .map(|name| name.replace('-', "_"))
        .collect();
    GPL_CONFIGURE_MARKERS
        .iter()
        .copied()
        .filter(|marker| enabled.iter().any(|e| e == marker))
        .collect()
}

/// Where a candidate binary came from, which decides how a failed hash check is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// A bundled sidecar or a `PATH` lookup — the shipped path. The pin is absolute here:
    /// no environment variable can make ANVIL run an unverified binary from these.
    Shipped,
    /// A path a developer put in `ANVIL_FFMPEG`. Still hash-checked, but they may opt out
    /// with [`ALLOW_UNPINNED_ENV`].
    DeveloperSupplied,
}

fn allow_unpinned() -> bool {
    matches!(
        std::env::var("ANVIL_FFMPEG_ALLOW_UNPINNED")
            .unwrap_or_default()
            .trim(),
        "1" | "true" | "TRUE"
    )
}

/// A located, integrity-checked ffmpeg binary, reusable for decode (and later encode).
#[derive(Debug, Clone)]
pub struct FfmpegSidecar {
    binary: PathBuf,
}

impl FfmpegSidecar {
    /// Locate ffmpeg without touching the network. Search order:
    /// 1. `ANVIL_FFMPEG` environment variable (explicit path),
    /// 2. a bundled sidecar next to the current executable (and, on macOS, the packaged `.app`'s
    ///    `../Resources/ffmpeg/` — see [`Self::candidates`]),
    /// 3. `ffmpeg` on `PATH`.
    ///
    /// The **first candidate that exists** is the one we commit to: if it fails the integrity
    /// check we fail loudly rather than quietly falling through to the next one, because a
    /// swapped bundled binary silently "working" off `PATH` is precisely the outcome the pin
    /// exists to prevent.
    ///
    /// Returns [`MediaError::SidecarNotFound`] if none exist (airplane-mode: we do not
    /// download it).
    pub fn locate() -> Result<Self, MediaError> {
        for (candidate, trust) in Self::candidates() {
            if candidate.is_file() {
                return Self::checked(candidate, trust);
            }
        }
        if let Some(found) = Self::search_path() {
            return Self::checked(found, Trust::Shipped);
        }
        Err(MediaError::SidecarNotFound(
            "no bundled sidecar, ANVIL_FFMPEG unset, and ffmpeg not on PATH \
             (airplane-mode: ANVIL never auto-downloads it)"
                .into(),
        ))
    }

    /// Wrap an explicit ffmpeg path, verifying its sha256 against this platform's pin
    /// ([`pinned_sha256`]).
    ///
    /// This is the strict entry point: the pin is enforced with no escape hatch, so a library
    /// caller cannot be tricked into running an unaudited binary. (The `ANVIL_FFMPEG`
    /// developer opt-out lives in [`Self::locate`], where a human is demonstrably present.)
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, MediaError> {
        Self::checked(path.into(), Trust::Shipped)
    }

    fn checked(binary: PathBuf, trust: Trust) -> Result<Self, MediaError> {
        if !binary.is_file() {
            return Err(MediaError::SidecarNotFound(binary.display().to_string()));
        }
        verify_hash(&binary, trust)?;
        Ok(Self { binary })
    }

    /// Path of the resolved binary.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The `configuration:` line from `ffmpeg -version` — the build's own record of which
    /// libraries it was compiled against.
    pub fn configure_line(&self) -> Result<String, MediaError> {
        let output = Command::new(&self.binary)
            .arg("-version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .find(|l| l.trim_start().starts_with("configuration:"))
            .map(|l| l.trim().to_string())
            .ok_or_else(|| {
                MediaError::SidecarFailed(format!(
                    "{} printed no `configuration:` line for -version",
                    self.binary.display()
                ))
            })
    }

    /// Fail unless this binary is a GPL-free build (see [`GPL_CONFIGURE_MARKERS`]).
    ///
    /// ANVIL is MIT and ships the sidecar; a GPL ffmpeg in the bundle is a licence violation,
    /// not a bug. Cheap enough (one `-version` spawn) to call at install/packaging time.
    pub fn assert_lgpl(&self) -> Result<(), MediaError> {
        let line = self.configure_line()?;
        let markers = gpl_markers_in(&line);
        if markers.is_empty() {
            return Ok(());
        }
        // A GPL sidecar in an MIT app is a licence violation, not a runtime fault; surface it
        // as a hard "we won't run this" with the offending markers named. Callers that need the
        // markers programmatically use [`gpl_markers_in`] directly.
        Err(MediaError::SidecarFailed(format!(
            "{} is a GPL ffmpeg build (configure enables: {}) — ANVIL ships LGPL only",
            self.binary.display(),
            markers.join(", ")
        )))
    }

    fn exe_name() -> String {
        // `EXE_SUFFIX` is ".exe" on Windows and "" elsewhere — cross-platform without any
        // `#[cfg]` (which the workspace confines to anvil-core::platform).
        format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)
    }

    fn candidates() -> Vec<(PathBuf, Trust)> {
        let mut out = Vec::new();
        if let Some(explicit) = std::env::var_os("ANVIL_FFMPEG") {
            out.push((PathBuf::from(explicit), Trust::DeveloperSupplied));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let name = Self::exe_name();
                out.push((dir.join(&name), Trust::Shipped));
                out.push((dir.join("sidecar").join(&name), Trust::Shipped));
                out.push((dir.join("ffmpeg").join(&name), Trust::Shipped));
                // In a packaged macOS `.app` the executable is at
                // `Anvil.app/Contents/MacOS/anvil`, but Tauri stages `bundle.resources` under
                // `Contents/Resources/` — so `Contents/MacOS/ffmpeg/` is empty and the sidecar
                // lives at `../Resources/ffmpeg/ffmpeg` relative to the exe (handoff/08-MAC.md §3).
                #[cfg(target_os = "macos")]
                out.push((dir.join("../Resources/ffmpeg").join(&name), Trust::Shipped));
            }
        }
        out
    }

    fn search_path() -> Option<PathBuf> {
        let name = Self::exe_name();
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(&name))
            .find(|candidate| candidate.is_file())
    }

    /// Probe container facts with ffmpeg by parsing the banner it writes to stderr for an
    /// `-i`-only invocation. Best-effort: ffmpeg's human-readable output is stable but not a
    /// contract, so callers should treat missing fields defensively.
    pub fn probe(&self, path: &Path) -> Result<MediaInfo, MediaError> {
        parse_ffmpeg_banner(&self.banner(path)?, path)
    }

    /// Run `ffmpeg -i <path>` with no output and capture the human-readable stderr banner
    /// (stream/format/chapter summary). Shared by [`Self::probe`] and
    /// [`crate::metadata::read_chapters`], which parses the same banner for the `Chapters:`
    /// block that lofty does not read (see the `metadata` module docs).
    pub(crate) fn banner(&self, path: &Path) -> Result<String, MediaError> {
        let output = Command::new(&self.binary)
            .args(["-hide_banner", "-i"])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        // `ffmpeg -i` with no output file exits non-zero by design; the stream info we want
        // is still on stderr, so we parse regardless of status.
        Ok(String::from_utf8_lossy(&output.stderr).into_owned())
    }

    /// Decode the whole file to one 48 kHz [`AudioBuffer`], reporting progress in `[0, 1]`.
    ///
    /// Channel count is preserved by forcing ffmpeg to the source's channel count (read via
    /// [`Self::probe`]) so de-interleaving the raw stream is unambiguous.
    pub fn decode_to_buffer(
        &self,
        path: &Path,
        mut progress: impl FnMut(f32),
    ) -> Result<AudioBuffer, MediaError> {
        let info = self.probe(path)?;
        let channels = (info.channels as usize).max(1);
        let total_secs = info.duration_secs;

        let mut child = self.spawn_decode(path, channels, true)?;
        let stdout = child.stdout.take().expect("stdout piped");

        // Drain stdout on a worker thread so a full stderr pipe (progress lines) can never
        // deadlock against a full stdout pipe.
        let reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut buf = Vec::new();
            let mut stdout = stdout;
            stdout.read_to_end(&mut buf)?;
            Ok(buf)
        });

        // Read progress + capture a stderr tail for diagnostics on the calling thread.
        let mut stderr_tail = String::new();
        if let Some(stderr) = child.stderr.take() {
            for line in BufReader::new(stderr).lines() {
                let line = line?;
                if let Some(frac) = progress_fraction(&line, total_secs) {
                    progress(frac);
                }
                push_tail(&mut stderr_tail, &line);
            }
        }

        let status = child.wait()?;
        let bytes = reader
            .join()
            .map_err(|_| MediaError::SidecarFailed("stdout reader thread panicked".into()))??;

        if !status.success() {
            return Err(MediaError::SidecarFailed(format!(
                "ffmpeg exited with {status}: {}",
                stderr_tail.trim()
            )));
        }

        let planar = deinterleave_f32le(&bytes, channels);
        progress(1.0);
        Ok(AudioBuffer::from_planar(planar, OUT_SAMPLE_RATE))
    }

    /// Open a streaming block decoder that reads raw `f32le` from ffmpeg incrementally,
    /// keeping only one block in memory at a time.
    pub fn decode_blocks(&self, path: &Path) -> Result<FfmpegBlocks, MediaError> {
        let info = self.probe(path)?;
        let channels = (info.channels as usize).max(1);
        // No `-progress` here and stderr is discarded, so nothing can back-pressure the
        // audio pipe as we consume it block by block.
        let mut child = self.spawn_decode(path, channels, false)?;
        let stdout = child.stdout.take().expect("stdout piped");
        Ok(FfmpegBlocks {
            child,
            stdout,
            channels,
            done: false,
        })
    }

    /// Build the shared decode command: raw planar-friendly `f32le` PCM on stdout, resampled
    /// to 48 kHz, source channel count forced for deterministic de-interleaving.
    fn spawn_decode(
        &self,
        path: &Path,
        channels: usize,
        progress: bool,
    ) -> Result<Child, MediaError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"])
            .arg("-i")
            .arg(path)
            .args(["-map", "0:a:0", "-vn"])
            .args(["-acodec", "pcm_f32le", "-f", "f32le"])
            .args(["-ac", &channels.to_string()])
            .args(["-ar", &OUT_SAMPLE_RATE.to_string()]);
        if progress {
            // Progress goes to stderr (pipe:2) because the raw audio owns stdout (pipe:1).
            cmd.args(["-progress", "pipe:2"]);
            cmd.stderr(Stdio::piped());
        } else {
            cmd.stderr(Stdio::null());
        }
        cmd.arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        cmd.spawn().map_err(MediaError::from)
    }
}

/// Streaming ffmpeg decoder: each [`Iterator::next`] reads up to one [`BLOCK_SAMPLES`]-frame
/// slab of raw `f32le` from the child and de-interleaves it.
pub struct FfmpegBlocks {
    child: Child,
    stdout: ChildStdout,
    channels: usize,
    done: bool,
}

impl FfmpegBlocks {
    /// Channel count of the decoded stream.
    pub fn channels(&self) -> usize {
        self.channels
    }

    fn finish(&mut self) -> Result<(), MediaError> {
        let status = self.child.wait()?;
        if !status.success() {
            return Err(MediaError::SidecarFailed(format!(
                "ffmpeg exited with {status}"
            )));
        }
        Ok(())
    }
}

impl Iterator for FfmpegBlocks {
    type Item = Result<AudioBuffer, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let bytes_per_block = BLOCK_SAMPLES * self.channels * 4;
        let mut buf = vec![0u8; bytes_per_block];

        let filled = match read_upto(&mut self.stdout, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(e.into()));
            }
        };

        if filled == 0 {
            self.done = true;
            return match self.finish() {
                Ok(()) => None,
                Err(e) => Some(Err(e)),
            };
        }

        // A short read is the final block; whole frames only.
        if filled < bytes_per_block {
            self.done = true;
            buf.truncate(filled - filled % (self.channels * 4));
        }

        let planar = deinterleave_f32le(&buf, self.channels);
        let block = AudioBuffer::from_planar(planar, OUT_SAMPLE_RATE);

        if self.done {
            if let Err(e) = self.finish() {
                return Some(Err(e));
            }
        }
        Some(Ok(block))
    }
}

impl Drop for FfmpegBlocks {
    fn drop(&mut self) {
        // If the consumer abandons us mid-stream, don't leave ffmpeg running.
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Verify a binary against this platform's [`FFMPEG_PINS`] entry before we execute it.
///
/// The hash compared depends on the platform. On **macOS** it is the signing-independent Mach-O
/// [content hash](macho_content_sha256): a Developer-ID re-sign rewrites the raw bytes of the
/// bundled sidecar (so its `binary_sha256` no longer matches), but not its *content*, so the
/// gate compares [`FfmpegPin::content_sha256`]. On **Windows/other** it stays the raw
/// [`sha256_file`] against [`FfmpegPin::binary_sha256`] (Authenticode gets the same treatment
/// when Windows signing lands — see ADR-012).
///
/// This is a hard check, not a warning: the only way past a mismatch is a developer who both
/// supplied the path via `ANVIL_FFMPEG` *and* set [`ALLOW_UNPINNED_ENV`]. See the module docs
/// for why the two sources are treated differently.
fn verify_hash(path: &Path, trust: Trust) -> Result<(), MediaError> {
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    let Some(pin) = current_pin() else {
        // No LGPL build vendored for this OS/arch yet. Refuse rather than run something we
        // have never audited — unless a developer explicitly took the wheel.
        let actual = sha256_file(path)?;
        if trust == Trust::DeveloperSupplied && allow_unpinned() {
            warn_unpinned(
                path,
                &actual,
                "(no pin for this platform)",
                "no pinned build exists for this platform",
            );
            return Ok(());
        }
        return Err(MediaError::SidecarFailed(format!(
            "no pinned ffmpeg build for {target}, so {} cannot be verified — ANVIL only runs a \
             hash-pinned LGPL sidecar (see anvil_media::sidecar::FFMPEG_PINS)",
            path.display()
        )));
    };

    // On macOS enforce the signing-independent content hash; elsewhere the raw file sha256. A
    // corrupt / non-Mach-O candidate on macOS falls back to its raw hash, which cannot match the
    // content pin, so it is reported as a uniform mismatch rather than a parse error.
    let on_macos = target.starts_with("macos-");
    let (expected, actual) = if on_macos {
        let actual = macho_content_sha256(path).or_else(|_| sha256_file(path))?;
        (pin.content_sha256, actual)
    } else {
        (Some(pin.binary_sha256), sha256_file(path)?)
    };

    let Some(expected) = expected else {
        // A macOS pin with no content hash recorded — refuse (defensive; every mac pin carries
        // one, kept honest by `pin_json_matches_the_code`).
        if trust == Trust::DeveloperSupplied && allow_unpinned() {
            warn_unpinned(
                path,
                &actual,
                "(no content pin for this platform)",
                "no content hash is pinned for this platform",
            );
            return Ok(());
        }
        return Err(MediaError::SidecarFailed(format!(
            "no content-hash pin for {target}, so {} cannot be verified — ANVIL only runs a \
             hash-pinned LGPL sidecar (see anvil_media::sidecar::FFMPEG_PINS)",
            path.display()
        )));
    };

    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    if trust == Trust::DeveloperSupplied && allow_unpinned() {
        warn_unpinned(
            path,
            &actual,
            expected,
            "does not match the pinned LGPL build",
        );
        return Ok(());
    }
    Err(MediaError::SidecarHashMismatch {
        expected: expected.to_string(),
        actual,
    })
}

/// The one place an unverified binary is tolerated — make it impossible to miss in a log.
fn warn_unpinned(path: &Path, actual: &str, expected: &str, why: &str) {
    tracing::warn!(
        binary = %path.display(),
        sha256 = %actual,
        expected = %expected,
        "RUNNING AN UNVERIFIED ffmpeg: it {why}, and {} is set. This is a developer escape \
         hatch. The binary may be a GPL build, which must never ship — see FFMPEG_PINS.",
        ALLOW_UNPINNED_ENV,
    );
}

/// Streaming sha256 of a file, lowercase hex. Streamed because the vendored static ffmpeg is
/// ~110 MB and this runs on every `locate()`.
///
/// Public so the packaging step can verify the binary it is about to bundle with the same code
/// path the app enforces at run time, rather than a second implementation that could drift.
pub fn sha256_file(path: &Path) -> Result<String, MediaError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

// ============================================================================================
// Signing-independent Mach-O content hash
// ============================================================================================
//
// WHY. A code signature is a trailing blob inside a Mach-O. When macOS `codesign` re-signs a
// binary (ad-hoc → Developer ID, or any re-sign), it rewrites that blob AND three header fields
// that describe its size — `__LINKEDIT.vmsize`, `__LINKEDIT.filesize`, and
// `LC_CODE_SIGNATURE.datasize` — so the *raw* sha256 of the bytes changes even though the actual
// program is byte-for-byte identical. That is the ship-blocker this module fixes: every bundled
// sidecar is Developer-ID re-signed at packaging time, so a raw-hash pin (recorded from the
// ad-hoc vendor binary) can never match the signed copy. Pinning the *content* — the file with
// the signature blob excluded and those three size fields neutralised — is stable across
// (re-)signing while still refusing any real content tamper (the code, data, symbol tables and
// load commands are all still hashed).
//
// ALGORITHM (parsed with `std` only — no new dependency).
//   * Thin 64-bit Mach-O (`MH_MAGIC_64` / byte-swapped `MH_CIGAM_64`): walk the load commands to
//     `LC_CODE_SIGNATURE`. With one present, sha256 over `[0, dataoff)` — the header + load
//     commands + real `__LINKEDIT` content — with `__LINKEDIT.vmsize`/`.filesize` and
//     `LC_CODE_SIGNATURE.datasize` zeroed, and the trailing signature blob `[dataoff, end)`
//     (plus any alignment padding after it) excluded. codesign always places the superblob at
//     the very end of `__LINKEDIT`, so excluding everything from `dataoff` is exactly the sig.
//     With NO `LC_CODE_SIGNATURE`, the content hash is the raw sha256 of the whole slice — so
//     unsigned/dev/Linux binaries keep identical semantics.
//   * FAT / universal (`FAT_MAGIC` / `FAT_MAGIC_64`, stored big-endian): sha256 over the STABLE
//     fat fields — `magic`, `nfat_arch`, and per-arch `cputype`/`cpusubtype`/`align` — followed
//     by each slice's own content hash, in fat-header order. The per-arch `offset`/`size` are
//     deliberately EXCLUDED: they shift when a slice is re-signed (each slice grows by its own
//     signature). They need no direct hashing — they only select which bytes each slice hash
//     already covers, so a tampered offset/size changes a slice hash or fails its bounds check.
//     (sherpa ships as one universal2 artifact whose x86_64 slice is unsigned upstream but is
//     signed by `codesign` at packaging time, so its pin is recorded from a fully-signed copy.)
//   * NOT for PE/Windows: this is only reached on macOS targets; Windows stays raw-file sha256.
//
// Every read is bounds-checked and returns an error (never panics) on a truncated or hostile
// input — see the `macho_*` unit tests, which fuzz offsets past EOF, overlapping ranges, and
// bad magics against hand-built fixtures.

/// 64-bit thin Mach-O magic (host-endian) and its byte-swapped form.
const MH_MAGIC_64: u32 = 0xFEED_FACF;
const MH_CIGAM_64: u32 = 0xCFFA_EDFE;
/// Universal ("fat") magics — always stored big-endian on disk, regardless of the slices.
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;
/// Load-command ids we care about (`<mach-o/loader.h>`).
const LC_SEGMENT_64: u32 = 0x19;
const LC_CODE_SIGNATURE: u32 = 0x1D;

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

fn rd_u32(data: &[u8], off: usize, e: Endian) -> Result<u32, String> {
    let end = off.checked_add(4).ok_or("offset overflow")?;
    let b: [u8; 4] = data
        .get(off..end)
        .ok_or("read past end of file")?
        .try_into()
        .map_err(|_| "slice")?;
    Ok(match e {
        Endian::Little => u32::from_le_bytes(b),
        Endian::Big => u32::from_be_bytes(b),
    })
}

fn rd_u64(data: &[u8], off: usize, e: Endian) -> Result<u64, String> {
    let end = off.checked_add(8).ok_or("offset overflow")?;
    let b: [u8; 8] = data
        .get(off..end)
        .ok_or("read past end of file")?
        .try_into()
        .map_err(|_| "slice")?;
    Ok(match e {
        Endian::Little => u64::from_le_bytes(b),
        Endian::Big => u64::from_be_bytes(b),
    })
}

/// The signing-independent content hash (lowercase hex) of the Mach-O — thin or universal — at
/// `path`. See the module section above for the algorithm and rationale; this is the hash the
/// macOS runtime gate ([`verify_hash`]) and the macOS pins ([`FfmpegPin::content_sha256`])
/// compare against. Public so packaging/tests can compute it with the exact code the gate uses.
pub fn macho_content_sha256(path: &Path) -> Result<String, MediaError> {
    let data = std::fs::read(path)?;
    let digest = macho_content_digest(&data).map_err(|e| {
        MediaError::SidecarFailed(format!(
            "{} is not a parseable Mach-O for content hashing ({e})",
            path.display()
        ))
    })?;
    Ok(hex_lower(&digest))
}

/// Dispatch thin vs FAT and return the 32-byte content digest.
fn macho_content_digest(data: &[u8]) -> Result<[u8; 32], String> {
    // The fat magic is stored big-endian regardless of the slices' own byte order; a thin binary
    // read big-endian yields `MH_CIGAM_64`/`MH_MAGIC_64`, neither of which is a fat magic, so it
    // correctly falls through to the thin path (which re-reads the magic to fix endianness).
    let magic_be = rd_u32(data, 0, Endian::Big)?;
    match magic_be {
        FAT_MAGIC | FAT_MAGIC_64 => fat_content_digest(data, magic_be == FAT_MAGIC_64),
        _ => thin_content_digest(data, 0, data.len()),
    }
}

/// Content digest of a universal binary (see the module section for the combination rule).
fn fat_content_digest(data: &[u8], is64: bool) -> Result<[u8; 32], String> {
    let nfat = rd_u32(data, 4, Endian::Big)? as usize;
    // A real universal binary has a handful of slices; cap so a hostile header can't make us spin.
    if nfat > 64 {
        return Err(format!("implausible fat_arch count {nfat}"));
    }
    let rec = if is64 { 32usize } else { 20 };
    let mut hasher = Sha256::new();
    hasher.update(&data[0..8]); // magic + nfat_arch (both stable; bounds already checked)
    let mut off = 8usize;
    let mut slices: Vec<(usize, usize)> = Vec::with_capacity(nfat);
    for _ in 0..nfat {
        let cputype = rd_u32(data, off, Endian::Big)?;
        let cpusubtype = rd_u32(data, off + 4, Endian::Big)?;
        let (soff, ssize, align) = if is64 {
            (
                rd_u64(data, off + 8, Endian::Big)? as usize,
                rd_u64(data, off + 16, Endian::Big)? as usize,
                rd_u32(data, off + 24, Endian::Big)?,
            )
        } else {
            (
                rd_u32(data, off + 8, Endian::Big)? as usize,
                rd_u32(data, off + 12, Endian::Big)? as usize,
                rd_u32(data, off + 16, Endian::Big)?,
            )
        };
        // Stable structure only — offset/size are excluded (they shift on re-sign).
        hasher.update(cputype.to_be_bytes());
        hasher.update(cpusubtype.to_be_bytes());
        hasher.update(align.to_be_bytes());
        slices.push((soff, ssize));
        off = off.checked_add(rec).ok_or("fat_arch table overflow")?;
    }
    for (soff, ssize) in slices {
        hasher.update(thin_content_digest(data, soff, ssize)?);
    }
    Ok(hasher.finalize().into())
}

/// Content digest of a thin 64-bit Mach-O occupying `data[base .. base+size]`.
fn thin_content_digest(data: &[u8], base: usize, size: usize) -> Result<[u8; 32], String> {
    let end = base.checked_add(size).ok_or("slice offset overflow")?;
    let slice = data.get(base..end).ok_or("slice past end of file")?;
    if slice.len() < 32 {
        return Err("Mach-O header truncated".into());
    }
    // Magic byte order fixes field endianness: bytes `CF FA ED FE` read little-endian are
    // `MH_MAGIC_64` (fields little-endian); `FE ED FA CF` read little-endian are `MH_CIGAM_64`
    // (fields big-endian).
    let m = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
    let endian = if m == MH_MAGIC_64 {
        Endian::Little
    } else if m == MH_CIGAM_64 {
        Endian::Big
    } else {
        return Err(format!("not a 64-bit Mach-O (magic {m:#010x})"));
    };
    let ncmds = rd_u32(slice, 16, endian)? as usize;
    let sizeofcmds = rd_u32(slice, 20, endian)? as usize;
    let lc_start = 32usize;
    let lc_end = lc_start
        .checked_add(sizeofcmds)
        .ok_or("sizeofcmds overflow")?;
    if lc_end > slice.len() {
        return Err("load commands extend past the slice".into());
    }

    let mut linkedit_vmsize_off: Option<usize> = None;
    let mut linkedit_filesize_off: Option<usize> = None;
    let mut sig_datasize_off: Option<usize> = None;
    let mut sig: Option<(usize, usize)> = None; // (dataoff, datasize), slice-relative

    let mut off = lc_start;
    for _ in 0..ncmds {
        if off + 8 > lc_end {
            return Err("load command header past the load-command region".into());
        }
        let cmd = rd_u32(slice, off, endian)?;
        let cmdsize = rd_u32(slice, off + 4, endian)? as usize;
        if cmdsize < 8 {
            return Err("load command size too small".into());
        }
        let cmd_end = off.checked_add(cmdsize).ok_or("load command overflow")?;
        if cmd_end > lc_end {
            return Err("load command extends past the load-command region".into());
        }
        if cmd == LC_SEGMENT_64 && cmdsize >= 72 {
            // segment_command_64: segname[16] @+8, vmsize (u64) @+32, filesize (u64) @+48.
            let name = &slice[off + 8..off + 24];
            let name = name.split(|&b| b == 0).next().unwrap_or(name);
            if name == b"__LINKEDIT" {
                linkedit_vmsize_off = Some(off + 32);
                linkedit_filesize_off = Some(off + 48);
            }
        } else if cmd == LC_CODE_SIGNATURE && cmdsize >= 16 {
            // linkedit_data_command: dataoff (u32) @+8, datasize (u32) @+12.
            let dataoff = rd_u32(slice, off + 8, endian)? as usize;
            let datasize = rd_u32(slice, off + 12, endian)? as usize;
            sig = Some((dataoff, datasize));
            sig_datasize_off = Some(off + 12);
        }
        off = cmd_end;
    }

    let mut hasher = Sha256::new();
    match sig {
        Some((dataoff, datasize)) => {
            let sig_end = dataoff
                .checked_add(datasize)
                .ok_or("code signature overflow")?;
            // The signature is the trailing blob: it must sit after the load commands and within
            // the slice. Everything from `dataoff` on (the sig plus any alignment padding) is
            // excluded; the three size fields it perturbs are zeroed in the hashed header.
            if dataoff < lc_end || sig_end > size {
                return Err("code signature is not a trailing blob (malformed)".into());
            }
            let mut header = slice[..lc_end].to_vec();
            for (field, width) in [
                (linkedit_vmsize_off, 8usize),
                (linkedit_filesize_off, 8),
                (sig_datasize_off, 4),
            ] {
                if let Some(o) = field {
                    if let Some(region) = header.get_mut(o..o + width) {
                        region.fill(0);
                    }
                }
            }
            hasher.update(&header);
            hasher.update(&slice[lc_end..dataoff]);
        }
        // No signature: the content hash is the raw hash of the whole slice.
        None => hasher.update(slice),
    }
    Ok(hasher.finalize().into())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Read until `buf` is full or EOF; returns bytes actually read (tolerates short reads).
fn read_upto(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// De-interleave little-endian f32 PCM into planar channels.
fn deinterleave_f32le(bytes: &[u8], channels: usize) -> Vec<Vec<f32>> {
    let channels = channels.max(1);
    let frames = bytes.len() / (4 * channels);
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        planar[i % channels].push(sample);
    }
    planar
}

/// Interleave a planar [`AudioBuffer`] into little-endian f32 PCM, the wire format ffmpeg
/// expects on stdin for encoding (`-f f32le`). The inverse of [`deinterleave_f32le`]; shared
/// by [`crate::encode`] and [`crate::video`] (video remux pipes the mastered audio the same
/// way decode reads it back out).
pub(crate) fn interleave_f32le(buffer: &AudioBuffer) -> Vec<u8> {
    let channels = buffer.channel_count().max(1);
    let frames = buffer.frames();
    let mut bytes = Vec::with_capacity(frames * channels * 4);
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buffer.channel(ch).get(frame).copied().unwrap_or(0.0);
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    bytes
}

/// Parse an ffmpeg `-progress` key/value line into a completion fraction, if it carries a
/// timestamp and we know the total duration. `pub(crate)`: reused by [`crate::encode`] and
/// [`crate::video`] to report progress on the write side the same way decode does on read.
pub(crate) fn progress_fraction(line: &str, total_secs: f64) -> Option<f32> {
    let (key, value) = line.split_once('=')?;
    match key.trim() {
        "progress" if value.trim() == "end" => Some(1.0),
        // `out_time_us` / `out_time_ms` are both microseconds in ffmpeg's output.
        "out_time_us" | "out_time_ms" if total_secs > 0.0 => {
            let micros: f64 = value.trim().parse().ok()?;
            let secs = micros / 1_000_000.0;
            Some((secs / total_secs).clamp(0.0, 1.0) as f32)
        }
        _ => None,
    }
}

pub(crate) fn push_tail(tail: &mut String, line: &str) {
    tail.push_str(line);
    tail.push('\n');
    // Keep only the last ~4 KB so a chatty run can't grow this unbounded.
    if tail.len() > 4096 {
        let cut = tail.len() - 4096;
        tail.drain(0..cut);
    }
}

/// Parse the human-readable stream banner ffmpeg prints for `-i`, extracting duration,
/// sample rate, channel count, and a container label.
fn parse_ffmpeg_banner(banner: &str, path: &Path) -> Result<MediaInfo, MediaError> {
    let mut duration_secs = 0.0;
    let mut source_sample_rate = 0u32;
    let mut channels = 0u16;
    let mut format = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".to_string());

    for line in banner.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("Input #0,") {
            // e.g. "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':"
            if let Some(name) = rest.split(',').next() {
                let name = name.trim();
                if !name.is_empty() {
                    format = name.to_string();
                }
            }
        }

        if let Some(idx) = trimmed.find("Duration:") {
            let after = &trimmed[idx + "Duration:".len()..];
            if let Some(token) = after.split(',').next() {
                if let Some(secs) = parse_hms(token.trim()) {
                    duration_secs = secs;
                }
            }
        }

        if trimmed.contains("Audio:") {
            if source_sample_rate == 0 {
                if let Some(rate) = find_before(trimmed, "Hz") {
                    source_sample_rate = rate as u32;
                }
            }
            if channels == 0 {
                channels = parse_channel_layout(trimmed);
            }
        }
    }

    if source_sample_rate == 0 {
        return Err(MediaError::UnsupportedFormat(format!(
            "ffmpeg reported no audio stream for {}",
            path.display()
        )));
    }

    Ok(MediaInfo {
        duration_secs,
        source_sample_rate,
        channels,
        format,
    })
}

/// Parse `HH:MM:SS.ss` into seconds.
fn parse_hms(token: &str) -> Option<f64> {
    let mut parts = token.split(':');
    let h: f64 = parts.next()?.trim().parse().ok()?;
    let m: f64 = parts.next()?.trim().parse().ok()?;
    let s: f64 = parts.next()?.trim().parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// Find the integer immediately preceding ` <marker>` (e.g. the number before "Hz").
fn find_before(line: &str, marker: &str) -> Option<u64> {
    let idx = line.find(marker)?;
    let head = line[..idx].trim_end();
    let digits: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

/// Map an ffmpeg channel-layout token to a channel count.
fn parse_channel_layout(line: &str) -> u16 {
    if line.contains("7.1") {
        return 8;
    }
    if line.contains("5.1") {
        return 6;
    }
    if line.contains("quad") {
        return 4;
    }
    if line.contains("stereo") {
        return 2;
    }
    if line.contains("mono") {
        return 1;
    }
    // e.g. "3 channels"
    line.split(',')
        .find_map(|seg| seg.trim().strip_suffix(" channels"))
        .and_then(|seg| seg.trim().parse::<u16>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The **real** configure line of the GPL ffmpeg used in dev (gyan.dev 6.1.1 "essentials",
    /// as shipped by npm `ffmpeg-static`). This is the binary that must never be released, and
    /// it is the negative fixture that proves the scanner actually catches a GPL build rather
    /// than vacuously passing everything.
    const GPL_CONFIGURE_LINE: &str = "configuration: --enable-gpl --enable-version3 --enable-static --pkg-config=pkgconf --disable-w32threads --disable-autodetect --enable-fontconfig --enable-iconv --enable-gnutls --enable-libxml2 --enable-gmp --enable-bzlib --enable-lzma --enable-zlib --enable-libsrt --enable-libssh --enable-libzmq --enable-avisynth --enable-sdl2 --enable-libwebp --enable-libx264 --enable-libx265 --enable-libxvid --enable-libaom --enable-libopenjpeg --enable-libvpx --enable-mediafoundation --enable-libass --enable-libfreetype --enable-libfribidi --enable-libharfbuzz --enable-libvidstab --enable-libvmaf --enable-libzimg --enable-amf --enable-cuda-llvm --enable-cuvid --enable-ffnvcodec --enable-nvdec --enable-nvenc --enable-dxva2 --enable-d3d11va --enable-libvpl --enable-libgme --enable-libopenmpt --enable-libopencore-amrwb --enable-libmp3lame --enable-libtheora --enable-libvo-amrwbenc --enable-libgsm --enable-libopencore-amrnb --enable-libopus --enable-libspeex --enable-libvorbis --enable-librubberband";

    /// EVERY pinned build must be GPL-free, and each must actually enable the external LGPL-safe
    /// codecs ANVIL relies on — including libass, which Clip Studio's caption burn-in requires on
    /// every platform (clip.rs renders through the `ass=` filter). The proof is mechanical, not
    /// cosmetic: ffmpeg's configure calls `die_license_disabled gpl` on every GPL-list library, so
    /// a build with no `--enable-gpl` provably links none of them.
    #[test]
    fn every_pinned_build_is_gpl_free_with_the_lgpl_codecs() {
        for p in FFMPEG_PINS {
            assert!(
                gpl_markers_in(p.configure_line).is_empty(),
                "{} is not GPL-free: {:?}",
                p.target,
                gpl_markers_in(p.configure_line)
            );
            assert!(!p.configure_line.contains("--enable-gpl"), "{}", p.target);
            assert!(
                !p.configure_line.contains("--enable-nonfree"),
                "{}",
                p.target
            );
            for need in [
                "--enable-libmp3lame",
                "--enable-libopus",
                "--enable-libvorbis",
                "--enable-libass",
            ] {
                assert!(
                    p.configure_line.contains(need),
                    "{} is missing {need}",
                    p.target
                );
            }
        }
    }

    /// Licence discipline per target: Windows is LGPLv3 (it sets `--enable-version3`, which is
    /// LGPL-not-GPL and must not read as a violation); the mac builds are deliberately stricter —
    /// LGPL-2.1, with NO `--enable-version3`.
    #[test]
    fn windows_is_lgplv3_and_mac_is_lgpl21() {
        let win = FFMPEG_PINS
            .iter()
            .find(|p| p.target == "windows-x86_64")
            .expect("a windows pin exists");
        assert_eq!(win.license, "LGPL-3.0-or-later");
        assert!(win.configure_line.contains("--enable-version3"));

        let macs: Vec<_> = FFMPEG_PINS
            .iter()
            .filter(|p| p.target.starts_with("macos-"))
            .collect();
        assert_eq!(macs.len(), 2, "arm64 + x86_64 mac pins");
        for p in macs {
            assert_eq!(p.license, "LGPL-2.1-or-later", "{}", p.target);
            assert!(
                !p.configure_line.contains("--enable-version3"),
                "{} must be stricter than Windows: no --enable-version3",
                p.target
            );
            assert!(
                p.configure_line.contains("--enable-videotoolbox"),
                "{} should enable videotoolbox (h264_videotoolbox is allowlisted in clip.rs)",
                p.target
            );
        }
    }

    #[test]
    fn the_dev_gpl_build_is_caught() {
        let markers = gpl_markers_in(GPL_CONFIGURE_LINE);
        for expected in [
            "gpl",
            "libx264",
            "libx265",
            "libxvid",
            "librubberband",
            "libvidstab",
            "avisynth",
        ] {
            assert!(
                markers.contains(&expected),
                "scanner missed {expected} in a real GPL build: {markers:?}"
            );
        }
    }

    /// A `--disable-` token must never read as a violation, or every LGPL build (which
    /// explicitly disables x264/x265) would be rejected.
    #[test]
    fn disabled_libraries_are_not_violations() {
        assert!(gpl_markers_in("configuration: --disable-libx264 --disable-gpl").is_empty());
        assert_eq!(
            gpl_markers_in("configuration: --enable-libx264"),
            ["libx264"]
        );
        // configure spells it `libfdk_aac`; the CLI flag uses a dash. Both must trip.
        assert_eq!(
            gpl_markers_in("configuration: --enable-libfdk-aac"),
            ["libfdk_aac"]
        );
    }

    /// Every table entry is structurally sound: unique target, well-formed lowercase-hex sha256, a
    /// non-empty configure line, and archive provenance consistent with its kind (a prebuilt entry
    /// has a URL + archive hash naming the version; a built-from-source entry has neither).
    #[test]
    fn pins_are_well_formed() {
        assert!(!FFMPEG_PINS.is_empty());
        let mut seen = std::collections::HashSet::new();
        let is_lc_sha256 = |s: &str| {
            s.len() == 64
                && s.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        };
        for p in FFMPEG_PINS {
            assert!(seen.insert(p.target), "duplicate target {}", p.target);
            assert!(is_lc_sha256(p.binary_sha256), "{} binary_sha256", p.target);
            assert!(!p.configure_line.is_empty(), "{} configure_line", p.target);

            // The content hash is the macOS runtime gate; every mac target carries one (a
            // well-formed lowercase-hex sha256), and non-mac targets carry none (they stay raw).
            match p.content_sha256 {
                Some(c) => {
                    assert!(
                        p.target.starts_with("macos-"),
                        "{} has a content_sha256 but is not a macOS target",
                        p.target
                    );
                    assert!(
                        is_lc_sha256(c),
                        "{} content_sha256 must be lowercase hex",
                        p.target
                    );
                    assert_ne!(
                        c, p.binary_sha256,
                        "{} content_sha256 must differ from the raw binary_sha256 \
                         (an ad-hoc-signed Mach-O's content hash excludes its signature)",
                        p.target
                    );
                }
                None => assert!(
                    !p.target.starts_with("macos-"),
                    "{} is a macOS target and must carry a content_sha256",
                    p.target
                ),
            }

            match p.archive_sha256 {
                Some(archive) => {
                    // Prebuilt: real archive, and the URL names the version it delivers.
                    assert_eq!(archive.len(), 64, "{} archive_sha256 len", p.target);
                    assert!(p.archive_member.is_some(), "{} archive_member", p.target);
                    assert!(
                        p.source_url.starts_with("https://"),
                        "{} source_url should be a URL",
                        p.target
                    );
                    assert!(
                        p.source_url.contains(p.version),
                        "{} source URL should name its version",
                        p.target
                    );
                }
                None => {
                    // Built-from-source: no archive; source_url is build-script provenance.
                    assert!(p.archive_member.is_none(), "{} archive_member", p.target);
                    assert!(
                        p.source_url.contains("built-from-source"),
                        "{} source_url should name the build script",
                        p.target
                    );
                }
            }
        }
    }

    /// The whole point of the pin: a binary that is not the audited build must not run.
    /// `from_path` is the strict door, so this holds regardless of the developer's env.
    #[test]
    fn a_binary_that_is_not_the_pinned_build_is_rejected() {
        let path = std::env::temp_dir().join(format!("anvil-ffmpeg-fake-{}", std::process::id()));
        std::fs::write(&path, b"I am not ffmpeg").expect("write fixture");

        let err = FfmpegSidecar::from_path(&path).expect_err("an impostor binary must be refused");
        let _ = std::fs::remove_file(&path);

        // On a platform with a vendored build (Windows, macOS) that's a hash mismatch; on one
        // with no pin yet (e.g. Linux) it's a refusal-to-run SidecarFailed. Either way: refused,
        // never executed.
        assert!(
            matches!(
                err,
                MediaError::SidecarHashMismatch { .. } | MediaError::SidecarFailed(_)
            ),
            "expected an integrity refusal, got {err:?}"
        );
    }

    /// [`current_pin`] / [`pinned_sha256`] resolve to the table entry for this exact platform, and
    /// to `None` on a platform we have not vendored (which is what makes every candidate refuse).
    #[test]
    fn pinned_sha256_tracks_the_table_for_this_platform() {
        let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        match FFMPEG_PINS.iter().find(|p| p.target == target) {
            Some(p) => {
                assert_eq!(current_pin(), Some(p));
                assert_eq!(pinned_sha256(), Some(p.binary_sha256));
            }
            None => {
                assert_eq!(current_pin(), None);
                assert_eq!(
                    pinned_sha256(),
                    None,
                    "no LGPL build has been vendored for {target} yet — it must not claim a pin"
                );
            }
        }
    }

    /// The schema's guardian. `scripts/ffmpeg-pin.json` drives the provisioning scripts and
    /// [`FFMPEG_PINS`] drives the loader; if they ever disagree, a script would install a binary
    /// the app then refuses. The JSON is a `targets` map keyed by `<os>-<arch>` (the code's runtime
    /// convention) with the GPL-marker list shared at top level. This test asserts EVERY table
    /// entry matches its JSON entry field-for-field, that neither side has an orphan target, and
    /// that the shared marker list is exactly [`GPL_CONFIGURE_MARKERS`].
    #[test]
    fn pin_json_matches_the_code() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/ffmpeg-pin.json")
            .canonicalize()
            .expect("scripts/ffmpeg-pin.json must exist — it is the provisioning source of truth");
        let raw = std::fs::read_to_string(&path).expect("read pin json");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("pin json must parse");

        let targets = json["targets"]
            .as_object()
            .expect("pin json must have a `targets` map (the multi-target schema)");

        // Every code entry has a matching JSON entry.
        for p in FFMPEG_PINS {
            let e = targets
                .get(p.target)
                .unwrap_or_else(|| panic!("pin json is missing target {}", p.target));
            let field = |k: &str| e.get(k).and_then(|v| v.as_str());
            assert_eq!(field("version"), Some(p.version), "{} version", p.target);
            assert_eq!(field("target"), Some(p.target), "{} target", p.target);
            assert_eq!(
                field("source_url"),
                Some(p.source_url),
                "{} source_url",
                p.target
            );
            assert_eq!(
                field("binary_sha256"),
                Some(p.binary_sha256),
                "{} binary_sha256",
                p.target
            );
            // The macOS runtime-gate hash: present (and equal) for mac targets, absent for others.
            assert_eq!(
                field("content_sha256"),
                p.content_sha256,
                "{} content_sha256 drift between ffmpeg-pin.json and FFMPEG_PINS",
                p.target
            );
            assert_eq!(field("license"), Some(p.license), "{} license", p.target);
            assert_eq!(
                field("license_file"),
                Some(p.license_file),
                "{} license_file",
                p.target
            );
            assert_eq!(
                field("configure_line"),
                Some(p.configure_line),
                "{} configure_line",
                p.target
            );
            // Optional archive fields: absent/null in JSON ⇔ None in code.
            assert_eq!(
                field("archive_sha256"),
                p.archive_sha256,
                "{} archive_sha256",
                p.target
            );
            assert_eq!(
                field("archive_member"),
                p.archive_member,
                "{} archive_member",
                p.target
            );
        }

        // No orphan JSON target the loader can't see.
        for key in targets.keys() {
            assert!(
                FFMPEG_PINS.iter().any(|p| p.target == key),
                "pin json target {key} has no FFMPEG_PINS entry"
            );
        }

        // The forbidden-marker list is shared at top level and must equal the code gate exactly.
        let markers: Vec<&str> = json["forbidden_configure_markers"]
            .as_array()
            .expect("forbidden_configure_markers must be a top-level array")
            .iter()
            .map(|v| v.as_str().expect("marker is a string"))
            .collect();
        assert_eq!(
            markers, GPL_CONFIGURE_MARKERS,
            "forbidden_configure_markers drifted from GPL_CONFIGURE_MARKERS"
        );
    }

    // --- Mach-O content hash (synthetic fixtures) -----------------------------------------
    //
    // These hand-build minimal but structurally valid Mach-Os so the parser is exercised with
    // no real binary present (the real vendor-vs-signed-app proof is the mac-gated integration
    // test in tests/sidecar_pin.rs). The invariants: a signature-size change must NOT change the
    // content hash; a body change MUST; an unsigned binary hashes as its raw bytes; and hostile
    // inputs error rather than panic.

    fn raw_digest(bytes: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().into()
    }

    /// Build a minimal little-endian thin arm64 Mach-O: a `__LINKEDIT` segment, optionally an
    /// `LC_CODE_SIGNATURE` whose trailing blob is `sig`, and `body` bytes as the "content".
    fn build_thin(body: &[u8], sig: Option<&[u8]>) -> Vec<u8> {
        let signed = sig.is_some();
        let sizeofcmds: u32 = 72 + if signed { 16 } else { 0 };
        let lc_end = 32 + sizeofcmds as usize;
        let dataoff = lc_end + body.len();
        let datasize = sig.map_or(0, <[u8]>::len);
        let file_end = dataoff + datasize;
        let le_fileoff = lc_end as u64;
        let le_filesize = file_end as u64 - le_fileoff;
        let le_vmsize = (le_filesize + 0x3fff) & !0x3fff;

        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        v.extend_from_slice(&0x0100_000Cu32.to_le_bytes()); // CPU_TYPE_ARM64
        v.extend_from_slice(&0u32.to_le_bytes()); // cpusubtype
        v.extend_from_slice(&2u32.to_le_bytes()); // MH_EXECUTE
        v.extend_from_slice(&(if signed { 2u32 } else { 1 }).to_le_bytes()); // ncmds
        v.extend_from_slice(&sizeofcmds.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // flags
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved
                                                  // LC_SEGMENT_64 __LINKEDIT (72 bytes)
        v.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
        v.extend_from_slice(&72u32.to_le_bytes());
        let mut segname = [0u8; 16];
        segname[..10].copy_from_slice(b"__LINKEDIT");
        v.extend_from_slice(&segname);
        v.extend_from_slice(&0u64.to_le_bytes()); // vmaddr
        v.extend_from_slice(&le_vmsize.to_le_bytes()); // vmsize (zeroed by the parser)
        v.extend_from_slice(&le_fileoff.to_le_bytes()); // fileoff
        v.extend_from_slice(&le_filesize.to_le_bytes()); // filesize (zeroed by the parser)
        v.extend_from_slice(&1i32.to_le_bytes()); // maxprot
        v.extend_from_slice(&1i32.to_le_bytes()); // initprot
        v.extend_from_slice(&0u32.to_le_bytes()); // nsects
        v.extend_from_slice(&0u32.to_le_bytes()); // flags
        if signed {
            v.extend_from_slice(&LC_CODE_SIGNATURE.to_le_bytes());
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&(dataoff as u32).to_le_bytes());
            v.extend_from_slice(&(datasize as u32).to_le_bytes());
        }
        assert_eq!(v.len(), lc_end, "load commands sized as declared");
        v.extend_from_slice(body);
        if let Some(s) = sig {
            v.extend_from_slice(s);
        }
        v
    }

    /// Build a big-endian universal binary out of already-built thin slices `(cputype, bytes)`.
    fn build_fat(slices: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let n = slices.len();
        let mut offsets = Vec::new();
        let mut cursor = 8 + n * 20;
        for (_, data) in slices {
            cursor = (cursor + 0x3fff) & !0x3fff; // 2^14 slice alignment
            offsets.push(cursor);
            cursor += data.len();
        }
        let mut v = Vec::new();
        v.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        v.extend_from_slice(&(n as u32).to_be_bytes());
        for (i, (cputype, data)) in slices.iter().enumerate() {
            v.extend_from_slice(&cputype.to_be_bytes());
            v.extend_from_slice(&0u32.to_be_bytes()); // cpusubtype
            v.extend_from_slice(&(offsets[i] as u32).to_be_bytes());
            v.extend_from_slice(&(data.len() as u32).to_be_bytes());
            v.extend_from_slice(&14u32.to_be_bytes()); // align 2^14
        }
        for (i, (_, data)) in slices.iter().enumerate() {
            v.resize(offsets[i], 0);
            v.extend_from_slice(data);
        }
        v
    }

    #[test]
    fn macho_unsigned_thin_hashes_as_raw_bytes() {
        // No LC_CODE_SIGNATURE ⇒ content hash == raw sha256 of the whole file (Linux/dev parity).
        let m = build_thin(b"the whole unsigned program body", None);
        assert_eq!(thin_content_digest(&m, 0, m.len()).unwrap(), raw_digest(&m));
        assert_eq!(macho_content_digest(&m).unwrap(), raw_digest(&m));
    }

    #[test]
    fn macho_content_hash_is_stable_across_signature_size() {
        // Same program, two DIFFERENT signatures (different length AND bytes) — as an ad-hoc vs a
        // Developer-ID signature would be. The content hash must be identical; the raw hash must
        // not (that difference is the ship-blocker bug).
        let body = b"identical program content across both signatures".as_slice();
        let adhoc = build_thin(body, Some(&[0xAAu8; 200]));
        let devid = build_thin(body, Some(&[0xBBu8; 9003])); // bigger, different bytes
        assert_eq!(
            thin_content_digest(&adhoc, 0, adhoc.len()).unwrap(),
            thin_content_digest(&devid, 0, devid.len()).unwrap(),
            "content hash must be stable across (re-)signing"
        );
        assert_ne!(
            raw_digest(&adhoc),
            raw_digest(&devid),
            "the raw hashes DO differ — that is exactly why raw pins broke on signing"
        );
    }

    #[test]
    fn macho_content_hash_detects_body_tamper() {
        let sig = [0xCDu8; 512];
        let honest = build_thin(b"honest program body ................", Some(&sig));
        let tampered = build_thin(b"HOSTILE program body ...............", Some(&sig));
        assert_ne!(
            thin_content_digest(&honest, 0, honest.len()).unwrap(),
            thin_content_digest(&tampered, 0, tampered.len()).unwrap(),
            "a content change must still be refused (this is the whole point of the pin)"
        );
    }

    #[test]
    fn macho_fat_content_hash_is_stable_when_a_slice_is_resigned() {
        // Two arches; re-sign slice 0 with a bigger signature (which shifts slice 1's offset and
        // slice 0's size in the fat header). Both are excluded from the hash, so the universal
        // binary's content hash is unchanged.
        let x86 = 0x0100_0007u32;
        let arm = 0x0100_000Cu32;
        let body0 = b"x86_64 slice program".as_slice();
        let body1 = b"arm64 slice program".as_slice();
        let a = build_fat(&[
            (x86, build_thin(body0, Some(&[0x11u8; 128]))),
            (arm, build_thin(body1, Some(&[0x22u8; 128]))),
        ]);
        let b = build_fat(&[
            (x86, build_thin(body0, Some(&[0x33u8; 4096]))), // slice 0 re-signed bigger
            (arm, build_thin(body1, Some(&[0x22u8; 128]))),
        ]);
        assert_ne!(
            raw_digest(&a),
            raw_digest(&b),
            "the two universal binaries really are different bytes (different signatures)"
        );
        assert_eq!(
            macho_content_digest(&a).unwrap(),
            macho_content_digest(&b).unwrap(),
            "a universal binary's content hash must survive a per-slice re-sign"
        );
        // And a real content change in a slice IS caught.
        let c = build_fat(&[
            (
                x86,
                build_thin(b"x86_64 slice PROGRAM", Some(&[0x11u8; 128])),
            ),
            (arm, build_thin(body1, Some(&[0x22u8; 128]))),
        ]);
        assert_ne!(
            macho_content_digest(&a).unwrap(),
            macho_content_digest(&c).unwrap()
        );
    }

    #[test]
    fn macho_hostile_inputs_error_rather_than_panic() {
        // Every one of these must return Err, never panic (bounds checks on all reads).
        assert!(macho_content_digest(&[]).is_err(), "empty");
        assert!(
            macho_content_digest(&[0u8; 4]).is_err(),
            "too small for header"
        );
        assert!(
            macho_content_digest(b"not a mach-o at all, just text.").is_err(),
            "bad magic"
        );

        // sizeofcmds claims more load commands than the file holds.
        let mut m = build_thin(b"body", Some(&[0u8; 64]));
        m[20] = 0xff;
        m[21] = 0xff; // enormous sizeofcmds
        assert!(macho_content_digest(&m).is_err(), "sizeofcmds past EOF");

        // A code signature whose dataoff points past EOF.
        let mut m = build_thin(b"body-body-body", Some(&[0u8; 32]));
        let n = m.len();
        // rewrite the LC_CODE_SIGNATURE dataoff (last 8 bytes of the load commands region) huge.
        let sig_dataoff_at = 32 + 72 + 8;
        m[sig_dataoff_at..sig_dataoff_at + 4].copy_from_slice(&(n as u32 + 4096).to_le_bytes());
        assert!(macho_content_digest(&m).is_err(), "dataoff past EOF");

        // FAT header with an implausible arch count and a slice offset past EOF.
        let mut fat = Vec::new();
        fat.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        fat.extend_from_slice(&9999u32.to_be_bytes());
        assert!(
            macho_content_digest(&fat).is_err(),
            "implausible nfat / truncated"
        );

        let mut fat2 = Vec::new();
        fat2.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        fat2.extend_from_slice(&1u32.to_be_bytes());
        fat2.extend_from_slice(&0x0100_000Cu32.to_be_bytes()); // cputype
        fat2.extend_from_slice(&0u32.to_be_bytes()); // cpusubtype
        fat2.extend_from_slice(&0x00ff_ffffu32.to_be_bytes()); // offset way past EOF
        fat2.extend_from_slice(&1024u32.to_be_bytes()); // size
        fat2.extend_from_slice(&14u32.to_be_bytes()); // align
        assert!(macho_content_digest(&fat2).is_err(), "fat slice past EOF");
    }
}
