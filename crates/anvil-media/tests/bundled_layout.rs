//! Proof that a **clean install** - the app exe with the bundled ffmpeg sidecar next to it and
//! no `ANVIL_*` environment at all - resolves ffmpeg exe-relative and passes the hash pin.
//!
//! This is the release-shape check for the packaging lane: `tauri.conf.json`'s
//! `bundle.resources` and `package-portable.mjs` both drop the vendored `ffmpeg/ffmpeg.exe`
//! into a `ffmpeg/` folder beside the executable, and [`FfmpegSidecar::locate`] must find it
//! there with zero configuration. We verify that by building a throwaway "install" directory
//! and **re-executing this very test binary from inside it** (so `std::env::current_exe()` - the
//! anchor `locate()` uses - points at the fake install), with every `ANVIL_*` variable stripped.
//!
//! On macOS the app is a `.app` bundle, where the sidecar lives under `Contents/Resources/ffmpeg/`
//! rather than next to the executable; [`app_bundle_resources_layout_resolves_ffmpeg_env_free`]
//! covers that shape with the same re-exec technique.
//!
//! Skips cleanly when the vendored ffmpeg has not been provisioned (run `scripts/fetch-ffmpeg.ps1`
//! on Windows, `scripts/build-ffmpeg-macos.sh` on macOS), exactly like `tests/sidecar_pin.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Set on the re-executed child to switch [`probe_locate_ffmpeg`] from "no-op suite member" to
/// "resolve the bundled sidecar and report".
const PROBE_ENV: &str = "ANVIL_MEDIA_BUNDLED_PROBE";

/// The vendored ffmpeg this machine would bundle, or `None` if not provisioned. Mirrors what
/// packaging copies next to the app. Resolves the per-platform vendor dir
/// (`vendor/ffmpeg/<os>-<arch>/`), so this is Windows' `windows-x86_64` and macOS'
/// `macos-aarch64` / `macos-x86_64` without hard-coding either.
fn vendored_ffmpeg() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ANVIL_FFMPEG") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/ffmpeg")
        .join(&target)
        .join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
    p.is_file().then_some(p)
}

/// The child half of the re-exec. In a normal `cargo test` run [`PROBE_ENV`] is unset and this
/// returns immediately. When the test re-runs this binary from the fake install with
/// [`PROBE_ENV`] set, it resolves ffmpeg with no `ANVIL_*` help and reports the path (or fails).
#[test]
fn probe_locate_ffmpeg() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    match anvil_media::sidecar::FfmpegSidecar::locate() {
        Ok(sidecar) => {
            // locate() only returns after verify_hash() passed, so reaching here proves the
            // bundled copy matches this platform's FFMPEG_PINS entry too.
            println!("PROBE_RESOLVED={}", sidecar.binary().display());
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("PROBE_FAILED={e}");
            std::process::exit(3);
        }
    }
}

#[test]
fn clean_install_layout_resolves_ffmpeg_env_free() {
    let Some(ffmpeg) = vendored_ffmpeg() else {
        eprintln!("skipping: no vendored ffmpeg (run scripts/fetch-ffmpeg.ps1)");
        return;
    };

    // Build a fake install: app.exe (a copy of this test binary) + ffmpeg/ffmpeg.exe (the real
    // bundled binary, so the hash pin is exercised for real, not stubbed).
    let install = std::env::temp_dir().join(format!(
        "anvil-media-clean-install-{}-{}",
        std::process::id(),
        nanos()
    ));
    let _ = std::fs::remove_dir_all(&install);
    std::fs::create_dir_all(install.join("ffmpeg")).expect("mk install/ffmpeg");

    let app = install.join(format!("app{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().expect("current_exe"), &app).expect("copy app exe");
    let bundled = install
        .join("ffmpeg")
        .join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&ffmpeg, &bundled).expect("copy bundled ffmpeg");

    // Re-run ONLY the probe test, from inside the install, with every ANVIL_* var removed so the
    // only way locate() can succeed is the exe-relative `ffmpeg/` folder.
    let mut cmd = Command::new(&app);
    cmd.args([
        "probe_locate_ffmpeg",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    for (k, _) in std::env::vars() {
        if k.starts_with("ANVIL_") {
            cmd.env_remove(k);
        }
    }
    cmd.env(PROBE_ENV, "1");
    let out = cmd.output().expect("re-exec app.exe");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let expected = format!("PROBE_RESOLVED={}", bundled.display());
    let _ = std::fs::remove_dir_all(&install);

    assert!(
        out.status.success(),
        "clean-install ffmpeg locate() failed (exit {:?}).\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains(&expected),
        "expected the exe-relative bundled ffmpeg to resolve.\nwanted: {expected}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// The macOS `.app` shape: the executable is at `Contents/MacOS/<app>` while Tauri stages the
/// sidecar under `Contents/Resources/ffmpeg/`, so [`FfmpegSidecar::locate`] must resolve it via the
/// `../Resources/ffmpeg/` fallback (handoff/08-MAC.md §3) with zero configuration, and the **real
/// built** binary must clear the hash pin. Mirrors [`clean_install_layout_resolves_ffmpeg_env_free`]
/// but for the resource-relative layout the exe-adjacent test does not exercise.
#[cfg(target_os = "macos")]
#[test]
fn app_bundle_resources_layout_resolves_ffmpeg_env_free() {
    let Some(ffmpeg) = vendored_ffmpeg() else {
        eprintln!("skipping: no vendored macOS ffmpeg (run scripts/build-ffmpeg-macos.sh)");
        return;
    };

    // Fake .app:  Anvil.app/Contents/MacOS/anvil  +  Contents/Resources/ffmpeg/ffmpeg (the real
    // bundled binary, so the hash pin is exercised for real, not stubbed).
    let install = std::env::temp_dir().join(format!(
        "anvil-media-appbundle-{}-{}",
        std::process::id(),
        nanos()
    ));
    let _ = std::fs::remove_dir_all(&install);
    let macos_dir = install.join("Anvil.app/Contents/MacOS");
    let res_dir = install.join("Anvil.app/Contents/Resources/ffmpeg");
    std::fs::create_dir_all(&macos_dir).expect("mk Contents/MacOS");
    std::fs::create_dir_all(&res_dir).expect("mk Contents/Resources/ffmpeg");

    let app = macos_dir.join("anvil");
    std::fs::copy(std::env::current_exe().expect("current_exe"), &app).expect("copy app exe");
    let bundled = res_dir.join("ffmpeg");
    std::fs::copy(&ffmpeg, &bundled).expect("copy bundled ffmpeg");

    // Re-run ONLY the probe test from inside the bundle with every ANVIL_* var removed, so the only
    // way locate() can succeed is the exe-relative `../Resources/ffmpeg/` fallback.
    let mut cmd = Command::new(&app);
    cmd.args([
        "probe_locate_ffmpeg",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    for (k, _) in std::env::vars() {
        if k.starts_with("ANVIL_") {
            cmd.env_remove(k);
        }
    }
    cmd.env(PROBE_ENV, "1");
    let out = cmd.output().expect("re-exec app exe");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // locate() joins a `..` segment (Contents/MacOS/../Resources/...), so compare the resolved path
    // CANONICALLY to the staged binary rather than by string equality. `--nocapture` prints the
    // marker mid-line (after the harness' "test probe_locate_ffmpeg ... "), so match the substring
    // anywhere, not just at line-start.
    let resolved = stdout
        .split("PROBE_RESOLVED=")
        .nth(1)
        .map(|rest| PathBuf::from(rest.lines().next().unwrap_or("").trim()));
    let resolves_to_staged =
        resolved.as_ref().and_then(|p| p.canonicalize().ok()) == bundled.canonicalize().ok();
    let _ = std::fs::remove_dir_all(&install);

    assert!(
        out.status.success(),
        "app-bundle ffmpeg locate() failed (exit {:?}).\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        resolves_to_staged,
        "expected ../Resources/ffmpeg to resolve to the staged binary.\nresolved: {resolved:?}\nstaged:   {}\nstdout:\n{stdout}",
        bundled.display()
    );
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
