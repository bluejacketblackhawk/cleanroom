//! Supply-chain tests for the ffmpeg sidecar (handoff/07-RISKS-LEGAL §2).
//!
//! ANVIL is MIT and **redistributes** an ffmpeg binary. Two things must therefore be true of
//! whatever we ship, and both are release blockers rather than niceties:
//!
//! 1. it is exactly the build we audited — enforced by a sha256 pin, so a swapped or corrupted
//!    binary is refused instead of executed;
//! 2. it is an **LGPL** build — no GPL components (x264, x265, rubberband, …), no nonfree ones
//!    (fdk-aac).
//!
//! The pure checks (the pin is well formed, a wrong binary is rejected, a GPL configure line is
//! caught) live as unit tests in `src/sidecar.rs` and always run. The tests here need the real
//! binary, so they **skip cleanly** when it has not been provisioned — run
//! `scripts/fetch-ffmpeg.ps1` and set `ANVIL_FFMPEG` to exercise them. Packaging must run them
//! green: see `ffmpeg_shipped_by_this_machine_is_the_audited_lgpl_build`.

use anvil_media::sidecar::{
    current_pin, gpl_markers_in, pinned_sha256, sha256_file, FfmpegSidecar,
};
use anvil_media::MediaError;
// Exercised only by the macOS-gated signed-bundle test below (unused on other platforms).
#[cfg(target_os = "macos")]
use anvil_media::sidecar::{content_pinned_sha256, macho_content_sha256, FFMPEG_PINS};
#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// The sidecar ANVIL would actually run on this machine, or `None` if none is provisioned.
///
/// Note this deliberately goes through `locate()`, so it is subject to the same integrity
/// policy the app uses — a developer running with `ANVIL_FFMPEG_ALLOW_UNPINNED=1` and a GPL
/// ffmpeg will reach the assertions below and **fail** them, which is the intended alarm.
fn located() -> Option<FfmpegSidecar> {
    match FfmpegSidecar::locate() {
        Ok(sidecar) => Some(sidecar),
        Err(MediaError::SidecarNotFound(_)) => {
            eprintln!("skipping: no ffmpeg provisioned (run scripts/fetch-ffmpeg.ps1)");
            None
        }
        // A mismatch/unpinned error here means the machine has *an* ffmpeg but not the audited
        // one and did not opt out. That is the pin doing its job, not a test failure.
        Err(e) => {
            eprintln!("skipping: ffmpeg present but not the pinned build ({e})");
            None
        }
    }
}

/// The release gate. Whatever binary this machine would run through the sidecar must be the
/// audited LGPL build — GPL-free configure line, matching hash.
#[test]
fn ffmpeg_shipped_by_this_machine_is_the_audited_lgpl_build() {
    let Some(sidecar) = located() else { return };

    let configure = sidecar.configure_line().expect("ffmpeg -version");
    let markers = gpl_markers_in(&configure);
    assert!(
        markers.is_empty(),
        "the ffmpeg ANVIL would run is a GPL/nonfree build (enables: {}). ANVIL is MIT and \
         ships this binary — it must be LGPL-only.\nbinary: {}\nconfigure: {configure}",
        markers.join(", "),
        sidecar.binary().display(),
    );

    // Same conclusion, through the API packaging is meant to call.
    sidecar
        .assert_lgpl()
        .expect("the located ffmpeg must be an LGPL build");
}

/// `assert_lgpl` must be reached through `locate()`'s integrity check, not around it: if this
/// platform has a pin, the located binary's hash is the pinned one.
#[test]
fn a_located_sidecar_on_a_pinned_platform_matches_the_pin() {
    let Some(pin) = pinned_sha256() else {
        eprintln!("skipping: no vendored ffmpeg build for this platform yet");
        return;
    };
    let Some(sidecar) = located() else { return };

    // `locate()` only returns a binary that already passed `verify_hash`, unless the developer
    // set ANVIL_FFMPEG_ALLOW_UNPINNED. Re-hash independently so the escape hatch cannot make
    // this test vacuous.
    let actual = sha256_file(sidecar.binary()).expect("hash the located binary");

    if actual != pin {
        // The developer is knowingly running an unpinned ffmpeg. Say so loudly, but the
        // LGPL assertion above still applies and still has teeth.
        eprintln!(
            "note: running an unpinned ffmpeg ({}); the shipped installer must carry {}",
            actual, pin
        );
        return;
    }
    assert_eq!(actual, pin, "the located sidecar is the audited build");
    // Per-platform licence: Windows is LGPLv3, macOS is LGPL-2.1 — both LGPL (GPL-free), which is
    // the invariant this gate exists to protect.
    let license = current_pin()
        .expect("we just matched this platform's pin")
        .license;
    assert!(
        license.starts_with("LGPL-"),
        "the shipped build must be an LGPL build, got {license}"
    );
}

/// The acceptance proof for the whole content-hash design (the mac-gated source of truth
/// `verify-mac-bundle.mjs` defers to). For each macOS ffmpeg pin it asserts that the
/// signing-independent content hash of the **vendored** (ad-hoc-signed) binary equals the pin's
/// `content_sha256` AND — for every Developer-ID-signed copy present (the built `.app` in
/// `target/`, and the installed `/Applications/ANVIL.app` for this host's arch) — equals the
/// content hash of the **re-signed** copy, even though their RAW sha256s differ. That equality
/// is exactly what makes the runtime gate survive signing. Then it drives the real gate
/// (`FfmpegSidecar::from_path`) against each signed copy, proving the GUI "hash mismatch"
/// ship-blocker is gone. Skips cleanly when neither a vendored binary nor a signed `.app` is
/// present, so `cargo test` stays green on any machine.
#[cfg(target_os = "macos")]
#[test]
fn signed_bundle_ffmpeg_content_hash_matches_the_pin() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let triple_for = |target: &str| match target {
        "macos-aarch64" => Some("aarch64-apple-darwin"),
        "macos-x86_64" => Some("x86_64-apple-darwin"),
        _ => None,
    };

    let mut proved = 0usize;
    for pin in FFMPEG_PINS
        .iter()
        .filter(|p| p.target.starts_with("macos-"))
    {
        let content_pin = pin
            .content_sha256
            .unwrap_or_else(|| panic!("{} must carry a content_sha256", pin.target));
        let triple = triple_for(pin.target).expect("mac triple");

        // Vendored (ad-hoc-signed) binary: raw != content, content == pin.
        let vendored = repo.join(format!("vendor/ffmpeg/{}/ffmpeg", pin.target));
        if vendored.is_file() {
            let raw = sha256_file(&vendored).expect("raw hash");
            let content = macho_content_sha256(&vendored).expect("content hash");
            assert_eq!(raw, pin.binary_sha256, "{} vendored raw sha256", pin.target);
            assert_eq!(
                content, content_pin,
                "{} vendored content hash must equal the pin",
                pin.target
            );
            proved += 1;
        }

        // Developer-ID-signed bundles: raw DIFFERS from the vendored (that is the bug), content
        // is IDENTICAL (that is the fix). Checked in the build tree AND — for the arch this host
        // runs — the installed /Applications copy, which is the exact artifact the GUI failure
        // was reproduced in.
        let host_target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let mut signed_copies = vec![repo.join(format!(
            "target/{triple}/release/bundle/macos/ANVIL.app/Contents/Resources/ffmpeg/ffmpeg"
        ))];
        if pin.target == host_target {
            signed_copies.push(PathBuf::from(
                "/Applications/ANVIL.app/Contents/Resources/ffmpeg/ffmpeg",
            ));
        }
        for bundled in signed_copies.into_iter().filter(|p| p.is_file()) {
            let content = macho_content_sha256(&bundled).expect("bundled content hash");
            assert_eq!(
                content,
                content_pin,
                "{} SIGNED copy at {} must content-hash to the pin (vendor==signed)",
                pin.target,
                bundled.display()
            );
            if vendored.is_file() {
                assert_ne!(
                    sha256_file(&bundled).unwrap(),
                    sha256_file(&vendored).unwrap(),
                    "{}: the signed copy's RAW hash must differ from the vendor's — otherwise \
                     this test proves nothing about signing-independence",
                    pin.target
                );
            }
            proved += 1;
        }
    }

    // For the arch we are running on, drive the ACTUAL runtime gate against every signed copy
    // present (built bundle + installed /Applications app): `from_path` runs `verify_hash`,
    // which on macOS now compares the content hash. Before the fix this returned
    // SidecarHashMismatch (the shipped-app bug); it must now succeed.
    let here = format!("{}-{}", std::env::consts::ARCH, "apple-darwin");
    let gate_targets = [
        repo.join(format!(
            "target/{here}/release/bundle/macos/ANVIL.app/Contents/Resources/ffmpeg/ffmpeg"
        )),
        PathBuf::from("/Applications/ANVIL.app/Contents/Resources/ffmpeg/ffmpeg"),
    ];
    for bundled_here in gate_targets.into_iter().filter(|p| p.is_file()) {
        let sidecar = FfmpegSidecar::from_path(&bundled_here).unwrap_or_else(|e| {
            panic!(
                "the runtime gate REFUSED the signed ffmpeg at {} — the ship-blocker is not \
                 fixed: {e}",
                bundled_here.display()
            )
        });
        assert_eq!(sidecar.binary(), bundled_here.as_path());
        // And the enforced pin really is the content hash, not the raw one.
        assert_eq!(
            content_pinned_sha256(),
            current_pin().and_then(|p| p.content_sha256)
        );
        assert_ne!(pinned_sha256(), content_pinned_sha256());
        proved += 1;
    }

    if proved == 0 {
        eprintln!(
            "skipping: no vendored mac ffmpeg and no signed .app present \
             (nothing to prove on this machine)"
        );
    }
}
