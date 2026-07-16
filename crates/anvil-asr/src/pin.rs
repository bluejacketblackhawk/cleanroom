//! Code-side provisioning pins for the whisper.cpp and sherpa-onnx sidecars, keyed by target.
//!
//! This mirrors [`anvil_media::FFMPEG_PIN`] — a per-target table plus a lookup by the runtime
//! `<os>-<arch>` string — but with one deliberate difference: anvil-asr **does not hash-check**
//! these binaries at run time. whisper.cpp is MIT and sherpa-onnx is Apache-2.0, so (unlike the
//! LGPL ffmpeg sidecar) there is no licence-control reason to gate execution on a hash; the pins
//! are the *provisioning-time* integrity contract (`scripts/whisper-pin.json`,
//! `scripts/sherpa-pin.json`, enforced by the fetch/build scripts).
//!
//! What this table buys us in code: a single guardian test (`pin_json_matches_the_code` in
//! `tests/bundled_layout.rs`) keeps these constants and the JSON pins in lockstep across every
//! target, and packaging / the bundled-layout test can look up the expected primary-binary
//! sha256 for the platform it runs on (see [`whisper_pinned_sha256`] / [`sherpa_pinned_sha256`],
//! the analogues of `anvil_media::sidecar::pinned_sha256`).
//!
//! The `<os>-<arch>` targets match [`std::env::consts`]: `windows-x86_64`, `macos-aarch64`,
//! `macos-x86_64`. The member dylib set (the *whole* audited bundle) lives in the JSON pins; this
//! code table carries only the version + the primary-binary hash, which is what a hash gate checks.
//!
//! ## macOS content hashes (signing-independent)
//! Each pin also carries a [`SidecarPin::content_sha256`] on macOS: the sha256 of the Mach-O with
//! its code-signature blob excluded (see [`macho_content_sha256`]). The bundled sidecars are
//! Developer-ID re-signed at packaging time, which rewrites their raw bytes, so any bundle-vs-pin
//! check on macOS must compare the *content* hash — the raw `binary_sha256` would never match the
//! signed `.app` copy. This is the same fix `anvil_media`'s ffmpeg gate uses; because `anvil-asr`
//! deliberately does **not** depend on `anvil-media` (it must not drag the media lane into the ASR
//! build), the small Mach-O parser is duplicated here rather than shared across a new crate edge.

use std::path::Path;

use sha2::{Digest, Sha256};

/// The pinned primary binary of one sidecar for one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarPin {
    /// `<os>-<arch>`, matching `format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)`.
    pub target: &'static str,
    /// Upstream version (git tag / release), e.g. `v1.9.1`.
    pub version: &'static str,
    /// Raw (provision-time) lowercase-hex sha256 of the primary binary (`whisper-cli` / the
    /// diarization exe): the hash the fetch/build/stage scripts verify the vendored bytes against.
    pub binary_sha256: &'static str,
    /// Signing-independent Mach-O **content** hash of that same binary (see
    /// [`macho_content_sha256`]) — `Some` for every macOS target, `None` for Windows/PE. A
    /// Developer-ID re-sign at packaging time rewrites the raw bytes but not the content, so this
    /// is the hash a mac bundle-vs-pin check must use (the raw `binary_sha256` would never match
    /// the signed `.app` copy). sherpa's macOS value is recorded from a **fully** signed copy
    /// because its upstream universal2 x86_64 slice is unsigned but `codesign` signs it at
    /// packaging time (see the mac note in `scripts/sherpa-pin.json`).
    pub content_sha256: Option<&'static str>,
    /// SPDX-ish licence of the engine binary.
    pub license: &'static str,
}

/// The whisper.cpp CLI sidecar, per target. Windows is the prebuilt release binary; the macOS
/// entries are this host's from-source build at the pinned tag (built-from-source is not
/// bit-reproducible across toolchains — see `scripts/build-whisper-macos.sh` and the `_comment`
/// in `scripts/whisper-pin.json`, which these values must equal).
pub const WHISPER_PINS: &[SidecarPin] = &[
    SidecarPin {
        target: "windows-x86_64",
        version: "v1.9.1",
        binary_sha256: "58245314fb73b30fbd0cf0542c5c172e23f02b6eb7cad7b51e792439cf5e1755",
        content_sha256: None,
        license: "MIT",
    },
    SidecarPin {
        target: "macos-aarch64",
        version: "v1.9.1",
        binary_sha256: "f0083e6b0911cfbf9ac3330f6d46ee822d76cf8059e822bc23705a47086def5d",
        content_sha256: Some("1d7b4f84e67f4a96ed7f7b2d599f494a386d5cc3807af4ca708f4d70e0e8a0d4"),
        license: "MIT",
    },
    SidecarPin {
        target: "macos-x86_64",
        version: "v1.9.1",
        binary_sha256: "34f456842f10862482185907fd5ec18495ce2d6fbb1973a1dbc532734bec9793",
        content_sha256: Some("f9ee50cace5f87594254d3c9e24ae8ab20198c3d5239189dd34f5c345adeb943"),
        license: "MIT",
    },
];

/// The sherpa-onnx speaker-diarization sidecar, per target. Windows is the `win-x64-shared`
/// build; the two macOS entries are the **same** `osx-universal2` fat binary staged per-arch, so
/// they deliberately carry identical `binary_sha256`s (see `scripts/fetch-sherpa-macos.sh`).
pub const SHERPA_PINS: &[SidecarPin] = &[
    SidecarPin {
        target: "windows-x86_64",
        version: "v1.12.14",
        binary_sha256: "65f6a5ceb5ecc5d5d706d61e78982dbb5943588cad2170b17673240f5b697712",
        content_sha256: None,
        license: "Apache-2.0",
    },
    SidecarPin {
        target: "macos-aarch64",
        version: "v1.12.14",
        binary_sha256: "08329a4b2d3098fa401ccfdc39ea75a45f9233bdbbeb6a0e01df7fe39d683f53",
        content_sha256: Some("bbe236573e5ba2d971907463641deb317c84c4324e893a8ff164bda5f5461d1b"),
        license: "Apache-2.0",
    },
    SidecarPin {
        target: "macos-x86_64",
        version: "v1.12.14",
        binary_sha256: "08329a4b2d3098fa401ccfdc39ea75a45f9233bdbbeb6a0e01df7fe39d683f53",
        content_sha256: Some("bbe236573e5ba2d971907463641deb317c84c4324e893a8ff164bda5f5461d1b"),
        license: "Apache-2.0",
    },
];

/// The `<os>-<arch>` string for the platform this build runs on (e.g. `"macos-aarch64"`). The
/// same expression `anvil_media::sidecar::pinned_sha256` uses, so the two lanes agree on target
/// naming by construction.
pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn find_for<'a>(pins: &'a [SidecarPin], target: &str) -> Option<&'a SidecarPin> {
    pins.iter().find(|p| p.target == target)
}

/// The whisper.cpp pin for the current platform, or `None` if no build is vendored for it yet.
pub fn whisper_pin() -> Option<&'static SidecarPin> {
    find_for(WHISPER_PINS, &current_target())
}

/// The sherpa-onnx pin for the current platform, or `None` if no build is vendored for it yet.
pub fn sherpa_pin() -> Option<&'static SidecarPin> {
    find_for(SHERPA_PINS, &current_target())
}

/// The pinned `whisper-cli` sha256 for the current platform (`None` off the vendored platforms).
/// The analogue of `anvil_media::sidecar::pinned_sha256`; unlike ffmpeg, ANVIL does not *enforce*
/// this at run time, but packaging and the bundled-layout test verify a staged binary against it.
pub fn whisper_pinned_sha256() -> Option<&'static str> {
    whisper_pin().map(|p| p.binary_sha256)
}

/// The pinned diarization-exe sha256 for the current platform (`None` off the vendored platforms).
pub fn sherpa_pinned_sha256() -> Option<&'static str> {
    sherpa_pin().map(|p| p.binary_sha256)
}

/// The pinned signing-independent `whisper-cli` **content** hash for the current platform, or
/// `None` on a platform whose pin carries none (Windows, or an unvendored OS). On macOS this is
/// the hash a bundle-vs-pin check must use — see [`macho_content_sha256`].
pub fn whisper_content_pinned_sha256() -> Option<&'static str> {
    whisper_pin().and_then(|p| p.content_sha256)
}

/// The pinned signing-independent diarization-exe **content** hash for the current platform, or
/// `None` on a platform whose pin carries none. On macOS this is the hash a bundle-vs-pin check
/// must use — see [`macho_content_sha256`].
pub fn sherpa_content_pinned_sha256() -> Option<&'static str> {
    sherpa_pin().and_then(|p| p.content_sha256)
}

// ============================================================================================
// Signing-independent Mach-O content hash
// ============================================================================================
//
// A DUPLICATE of `anvil_media::sidecar`'s parser, kept private to this crate on purpose: making
// `anvil-asr` depend on `anvil-media` just to share ~150 lines would drag the entire media lane
// (symphonia, rubato, lofty, the ffmpeg sidecar) into every crate that builds ASR — a cost far
// larger than the duplication. The algorithm is identical; see the extended commentary in
// `crates/anvil-media/src/sidecar.rs`. In short: exclude the trailing code-signature blob and
// zero the three header fields that describe its size (`__LINKEDIT.vmsize`/`.filesize` and
// `LC_CODE_SIGNATURE.datasize`), so the hash is stable across ad-hoc/Developer-ID/re-signing but
// still refuses any content tamper; FAT binaries hash the stable fat fields plus each slice's
// content hash. Every read is bounds-checked and errors (never panics) on hostile input.

const MH_MAGIC_64: u32 = 0xFEED_FACF;
const MH_CIGAM_64: u32 = 0xCFFA_EDFE;
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;
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
/// `path`. The macOS analogue of the raw sha256 the fetch/build scripts pin; a bundle-vs-pin
/// check compares this against [`SidecarPin::content_sha256`]. Errors (never panics) on a file
/// that is not a parseable Mach-O.
pub fn macho_content_sha256(path: &Path) -> Result<String, crate::error::AsrError> {
    let data = std::fs::read(path)?;
    let digest = macho_content_digest(&data).map_err(|e| {
        crate::error::AsrError::SidecarFailed(format!(
            "{} is not a parseable Mach-O for content hashing ({e})",
            path.display()
        ))
    })?;
    Ok(hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn macho_content_digest(data: &[u8]) -> Result<[u8; 32], String> {
    let magic_be = rd_u32(data, 0, Endian::Big)?;
    match magic_be {
        FAT_MAGIC | FAT_MAGIC_64 => fat_content_digest(data, magic_be == FAT_MAGIC_64),
        _ => thin_content_digest(data, 0, data.len()),
    }
}

fn fat_content_digest(data: &[u8], is64: bool) -> Result<[u8; 32], String> {
    let nfat = rd_u32(data, 4, Endian::Big)? as usize;
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

fn thin_content_digest(data: &[u8], base: usize, size: usize) -> Result<[u8; 32], String> {
    let end = base.checked_add(size).ok_or("slice offset overflow")?;
    let slice = data.get(base..end).ok_or("slice past end of file")?;
    if slice.len() < 32 {
        return Err("Mach-O header truncated".into());
    }
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
    let mut sig: Option<(usize, usize)> = None;

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
            let name = &slice[off + 8..off + 24];
            let name = name.split(|&b| b == 0).next().unwrap_or(name);
            if name == b"__LINKEDIT" {
                linkedit_vmsize_off = Some(off + 32);
                linkedit_filesize_off = Some(off + 48);
            }
        } else if cmd == LC_CODE_SIGNATURE && cmdsize >= 16 {
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
        None => hasher.update(slice),
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_sha256(s: &str) -> bool {
        s.len() == 64
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    }

    #[test]
    fn pin_tables_are_well_formed_and_unique() {
        for pins in [WHISPER_PINS, SHERPA_PINS] {
            let mut targets: Vec<&str> = pins.iter().map(|p| p.target).collect();
            let n = targets.len();
            targets.sort_unstable();
            targets.dedup();
            assert_eq!(targets.len(), n, "sidecar pin targets must be unique");
            for p in pins {
                assert!(is_sha256(p.binary_sha256), "{} bad sha256", p.target);
                assert!(
                    !p.version.is_empty() && !p.license.is_empty(),
                    "{}",
                    p.target
                );
                // Every macOS target carries a well-formed content hash (the bundle-vs-pin gate);
                // non-mac targets carry none (they stay raw).
                match p.content_sha256 {
                    Some(c) => {
                        assert!(
                            p.target.starts_with("macos-"),
                            "{} has a content_sha256 but is not macOS",
                            p.target
                        );
                        assert!(is_sha256(c), "{} bad content_sha256", p.target);
                    }
                    None => assert!(
                        !p.target.starts_with("macos-"),
                        "{} is macOS and must carry a content_sha256",
                        p.target
                    ),
                }
            }
        }
        // Both sidecars must cover the same target matrix (windows + the two mac arches).
        let ws: Vec<&str> = WHISPER_PINS.iter().map(|p| p.target).collect();
        let ss: Vec<&str> = SHERPA_PINS.iter().map(|p| p.target).collect();
        assert_eq!(
            ws, ss,
            "whisper and sherpa must be vendored for the same targets"
        );
    }

    /// The two macOS sherpa entries are the one universal2 artifact staged twice — same hash.
    #[test]
    fn sherpa_mac_entries_share_the_universal2_hash() {
        let a = find_for(SHERPA_PINS, "macos-aarch64").unwrap();
        let x = find_for(SHERPA_PINS, "macos-x86_64").unwrap();
        assert_eq!(a.binary_sha256, x.binary_sha256);
    }

    /// On a vendored platform the lookup resolves; the hash it returns is the primary-binary hash.
    #[test]
    fn lookup_resolves_on_this_platform_if_vendored() {
        let t = current_target();
        match whisper_pin() {
            Some(p) => {
                assert_eq!(p.target, t);
                assert_eq!(whisper_pinned_sha256(), Some(p.binary_sha256));
            }
            None => assert_eq!(whisper_pinned_sha256(), None),
        }
    }

    /// The two macOS sherpa entries are one universal2 artifact — the *content* hashes must match
    /// too, not just the raw ones (both slices are signed identically at packaging time).
    #[test]
    fn sherpa_mac_entries_share_the_content_hash() {
        let a = find_for(SHERPA_PINS, "macos-aarch64").unwrap();
        let x = find_for(SHERPA_PINS, "macos-x86_64").unwrap();
        assert_eq!(a.content_sha256, x.content_sha256);
        assert!(a.content_sha256.is_some());
    }

    // --- Mach-O content hash (synthetic fixtures) -----------------------------------------
    // The duplicate parser gets its own independent tests: a signature-size change must not move
    // the content hash, a body change must, an unsigned binary hashes as raw, and hostile inputs
    // error rather than panic. (The vendor-vs-signed-app proof against real binaries is the
    // mac-gated integration test in tests/bundled_layout.rs.)

    fn raw_digest(bytes: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().into()
    }

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
        v.extend_from_slice(&0x0100_000Cu32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&(if signed { 2u32 } else { 1 }).to_le_bytes());
        v.extend_from_slice(&sizeofcmds.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
        v.extend_from_slice(&72u32.to_le_bytes());
        let mut segname = [0u8; 16];
        segname[..10].copy_from_slice(b"__LINKEDIT");
        v.extend_from_slice(&segname);
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&le_vmsize.to_le_bytes());
        v.extend_from_slice(&le_fileoff.to_le_bytes());
        v.extend_from_slice(&le_filesize.to_le_bytes());
        v.extend_from_slice(&1i32.to_le_bytes());
        v.extend_from_slice(&1i32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        if signed {
            v.extend_from_slice(&LC_CODE_SIGNATURE.to_le_bytes());
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&(dataoff as u32).to_le_bytes());
            v.extend_from_slice(&(datasize as u32).to_le_bytes());
        }
        assert_eq!(v.len(), lc_end);
        v.extend_from_slice(body);
        if let Some(s) = sig {
            v.extend_from_slice(s);
        }
        v
    }

    fn build_fat(slices: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let n = slices.len();
        let mut offsets = Vec::new();
        let mut cursor = 8 + n * 20;
        for (_, data) in slices {
            cursor = (cursor + 0x3fff) & !0x3fff;
            offsets.push(cursor);
            cursor += data.len();
        }
        let mut v = Vec::new();
        v.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        v.extend_from_slice(&(n as u32).to_be_bytes());
        for (i, (cputype, data)) in slices.iter().enumerate() {
            v.extend_from_slice(&cputype.to_be_bytes());
            v.extend_from_slice(&0u32.to_be_bytes());
            v.extend_from_slice(&(offsets[i] as u32).to_be_bytes());
            v.extend_from_slice(&(data.len() as u32).to_be_bytes());
            v.extend_from_slice(&14u32.to_be_bytes());
        }
        for (i, (_, data)) in slices.iter().enumerate() {
            v.resize(offsets[i], 0);
            v.extend_from_slice(data);
        }
        v
    }

    #[test]
    fn macho_unsigned_thin_hashes_as_raw_bytes() {
        let m = build_thin(b"the whole unsigned program body", None);
        assert_eq!(macho_content_digest(&m).unwrap(), raw_digest(&m));
    }

    #[test]
    fn macho_content_hash_is_stable_across_signature_size() {
        let body = b"identical program content across both signatures".as_slice();
        let adhoc = build_thin(body, Some(&[0xAAu8; 200]));
        let devid = build_thin(body, Some(&[0xBBu8; 9003]));
        assert_eq!(
            macho_content_digest(&adhoc).unwrap(),
            macho_content_digest(&devid).unwrap()
        );
        assert_ne!(raw_digest(&adhoc), raw_digest(&devid));
    }

    #[test]
    fn macho_content_hash_detects_body_tamper() {
        let sig = [0xCDu8; 512];
        let honest = build_thin(b"honest program body ................", Some(&sig));
        let tampered = build_thin(b"HOSTILE program body ...............", Some(&sig));
        assert_ne!(
            macho_content_digest(&honest).unwrap(),
            macho_content_digest(&tampered).unwrap()
        );
    }

    #[test]
    fn macho_fat_content_hash_is_stable_when_a_slice_is_resigned() {
        let x86 = 0x0100_0007u32;
        let arm = 0x0100_000Cu32;
        let body0 = b"x86_64 slice program".as_slice();
        let body1 = b"arm64 slice program".as_slice();
        let a = build_fat(&[
            (x86, build_thin(body0, Some(&[0x11u8; 128]))),
            (arm, build_thin(body1, Some(&[0x22u8; 128]))),
        ]);
        let b = build_fat(&[
            (x86, build_thin(body0, Some(&[0x33u8; 4096]))),
            (arm, build_thin(body1, Some(&[0x22u8; 128]))),
        ]);
        assert_ne!(raw_digest(&a), raw_digest(&b));
        assert_eq!(
            macho_content_digest(&a).unwrap(),
            macho_content_digest(&b).unwrap()
        );
    }

    #[test]
    fn macho_hostile_inputs_error_rather_than_panic() {
        assert!(macho_content_digest(&[]).is_err());
        assert!(macho_content_digest(&[0u8; 4]).is_err());
        assert!(macho_content_digest(b"not a mach-o at all, just text.").is_err());
        let mut m = build_thin(b"body", Some(&[0u8; 64]));
        m[20] = 0xff;
        m[21] = 0xff;
        assert!(macho_content_digest(&m).is_err());
        let mut fat = Vec::new();
        fat.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        fat.extend_from_slice(&9999u32.to_be_bytes());
        assert!(macho_content_digest(&fat).is_err());
    }
}
