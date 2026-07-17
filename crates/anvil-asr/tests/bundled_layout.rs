//! Proof that a **clean install** resolves the whisper.cpp and sherpa-onnx sidecars
//! exe-relative with no `CLEANROOM_*` environment, plus guards keeping the provisioning pins
//! (`scripts/whisper-pin.json`, `scripts/sherpa-pin.json`) honest against the app.
//!
//! Packaging drops the whisper bundle into a `whisper/` folder and the sherpa bundle into a
//! `sherpa/` folder next to the executable (`tauri.conf.json` `bundle.resources` +
//! `package-portable.mjs`). [`WhisperSidecar::locate`] and [`DiarizeSidecar::locate`] must find
//! them there env-free. Unlike ffmpeg these locators do not hash-check at run time (whisper is
//! MIT, sherpa Apache-2.0 - no licence-control reason to), so the pin is a *provisioning-time*
//! gate; the resolution test therefore only needs the binaries to exist at the right path, and
//! uses stand-in files so it runs on any machine without provisioning.

use std::path::PathBuf;
use std::process::Command;

const PROBE_ENV: &str = "CLEANROOM_ASR_BUNDLED_PROBE";

/// Child half of the re-exec: resolve both sidecars with no `CLEANROOM_*` help and report. A normal
/// `cargo test` run (no [`PROBE_ENV`]) returns immediately.
#[test]
fn probe_locate_sidecars() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    let whisper = anvil_asr::WhisperSidecar::locate();
    let diarize = anvil_asr::DiarizeSidecar::locate();
    match (whisper, diarize) {
        (Ok(w), Ok(d)) => {
            println!("PROBE_WHISPER={}", w.binary().display());
            println!("PROBE_SHERPA={}", d.binary().display());
            std::process::exit(0);
        }
        (w, d) => {
            eprintln!("PROBE_FAILED whisper={:?} sherpa={:?}", w.err(), d.err());
            std::process::exit(3);
        }
    }
}

#[test]
fn clean_install_layout_resolves_whisper_and_sherpa_env_free() {
    let install = std::env::temp_dir().join(format!(
        "anvil-asr-clean-install-{}-{}",
        std::process::id(),
        nanos()
    ));
    let _ = std::fs::remove_dir_all(&install);

    let exe = |name: &str| format!("{name}{}", std::env::consts::EXE_SUFFIX);
    // The exe-relative folders locate() checks last (dir/whisper/… and dir/sherpa/…), which is
    // exactly where packaging places the bundles.
    let whisper_dir = install.join("whisper");
    let sherpa_dir = install.join("sherpa");
    std::fs::create_dir_all(&whisper_dir).expect("mk whisper dir");
    std::fs::create_dir_all(&sherpa_dir).expect("mk sherpa dir");

    let app = install.join(exe("app"));
    std::fs::copy(std::env::current_exe().expect("current_exe"), &app).expect("copy app exe");
    // Stand-ins: locate() checks existence only, so real binaries are unnecessary here (they are
    // proven to run by the fetch scripts' smoke tests). This keeps the test hermetic.
    let whisper_bin = whisper_dir.join(exe("whisper-cli"));
    let sherpa_bin = sherpa_dir.join(exe("sherpa-onnx-offline-speaker-diarization"));
    std::fs::write(&whisper_bin, b"stub").expect("write whisper stub");
    std::fs::write(&sherpa_bin, b"stub").expect("write sherpa stub");

    let mut cmd = Command::new(&app);
    cmd.args([
        "probe_locate_sidecars",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    for (k, _) in std::env::vars() {
        if k.starts_with("CLEANROOM_") {
            cmd.env_remove(k);
        }
    }
    cmd.env(PROBE_ENV, "1");
    let out = cmd.output().expect("re-exec app.exe");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let want_whisper = format!("PROBE_WHISPER={}", whisper_bin.display());
    let want_sherpa = format!("PROBE_SHERPA={}", sherpa_bin.display());
    let _ = std::fs::remove_dir_all(&install);

    assert!(
        out.status.success(),
        "clean-install locate() failed (exit {:?}).\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains(&want_whisper),
        "whisper did not resolve exe-relative.\nwanted: {want_whisper}\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&want_sherpa),
        "sherpa did not resolve exe-relative.\nwanted: {want_sherpa}\nstdout:\n{stdout}"
    );
}

/// The macOS `.app` layout, which the flat test above does not cover: the exe lives at
/// `Contents/MacOS/<app>` and the sidecars are bundle resources under `Contents/Resources/` —
/// whisper flat (`Resources/whisper/whisper-cli`), sherpa structured with its `bin/` + `lib/`
/// siblings (`Resources/sherpa/bin/<exe>`). Proves [`WhisperSidecar::locate`] and
/// [`DiarizeSidecar::locate`] resolve both from `../Resources/` with `CLEANROOM_*` stripped, AND
/// exercises the provisioning hash gate: the staged binaries are the REAL vendored mac builds, so
/// their RAW sha256 must equal the code-side pins ([`anvil_asr::whisper_pinned_sha256`] /
/// [`anvil_asr::sherpa_pinned_sha256`]). Skips cleanly off macOS or when `vendor/` (gitignored)
/// has not been provisioned, so `cargo test` stays green everywhere.
///
/// This checks the *raw* hash on purpose — the vendored binaries are exactly the raw-pinned
/// artifacts. The signing-independent CONTENT-hash bundle-vs-pin proof (vendored vs the
/// Developer-ID-signed `.app`, which have DIFFERENT raw hashes but the same content) lives in
/// [`signed_bundle_content_hash_matches_the_pins`] below.
#[test]
fn macos_app_bundle_layout_resolves_from_resources_with_hash_gate() {
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    if !target.starts_with("macos-") {
        eprintln!("skipping macOS .app layout test on {target}");
        return;
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let real_whisper = repo.join(format!("vendor/whisper/{target}/whisper-cli"));
    let real_sherpa = repo.join(format!(
        "vendor/sherpa/{target}/bin/sherpa-onnx-offline-speaker-diarization"
    ));
    if !real_whisper.is_file() || !real_sherpa.is_file() {
        eprintln!(
            "skipping macOS .app layout test: sidecars not provisioned for {target} \
             (run scripts/build-whisper-macos.sh + scripts/fetch-sherpa-macos.sh)"
        );
        return;
    }

    // Build a fake Anvil.app/Contents/{MacOS,Resources/...} tree from the real vendored binaries.
    let app_root =
        std::env::temp_dir().join(format!("anvil-asr-app-{}-{}", std::process::id(), nanos()));
    let _ = std::fs::remove_dir_all(&app_root);
    let macos_dir = app_root.join("Contents/MacOS");
    let res_whisper = app_root.join("Contents/Resources/whisper");
    let res_sherpa_bin = app_root.join("Contents/Resources/sherpa/bin");
    std::fs::create_dir_all(&macos_dir).expect("mk MacOS");
    std::fs::create_dir_all(&res_whisper).expect("mk Resources/whisper");
    std::fs::create_dir_all(&res_sherpa_bin).expect("mk Resources/sherpa/bin");

    let app = macos_dir.join("anvil");
    std::fs::copy(std::env::current_exe().expect("current_exe"), &app).expect("copy app");
    let staged_whisper = res_whisper.join("whisper-cli");
    let staged_sherpa = res_sherpa_bin.join("sherpa-onnx-offline-speaker-diarization");
    std::fs::copy(&real_whisper, &staged_whisper).expect("copy whisper-cli");
    std::fs::copy(&real_sherpa, &staged_sherpa).expect("copy sherpa exe");

    let mut cmd = Command::new(&app);
    cmd.args([
        "probe_locate_sidecars",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    for (k, _) in std::env::vars() {
        if k.starts_with("CLEANROOM_") {
            cmd.env_remove(k);
        }
    }
    cmd.env(PROBE_ENV, "1");
    let out = cmd.output().expect("re-exec app");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // locate() returns the `..`-relative path it builds (`Contents/MacOS/../Resources/…`), so the
    // expected strings are constructed the same way rather than canonicalised.
    let want_whisper = format!(
        "PROBE_WHISPER={}",
        macos_dir.join("../Resources/whisper/whisper-cli").display()
    );
    let want_sherpa = format!(
        "PROBE_SHERPA={}",
        macos_dir
            .join("../Resources/sherpa/bin/sherpa-onnx-offline-speaker-diarization")
            .display()
    );

    // Hash gate: the staged (== vendored) binaries must match the code-side pins for this target.
    let want_wsha = anvil_asr::whisper_pinned_sha256().expect("a whisper pin for this mac target");
    let want_ssha = anvil_asr::sherpa_pinned_sha256().expect("a sherpa pin for this mac target");
    let got_wsha = sha256_file(&staged_whisper);
    let got_ssha = sha256_file(&staged_sherpa);

    let _ = std::fs::remove_dir_all(&app_root);

    assert!(
        out.status.success(),
        ".app locate() failed (exit {:?}).\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains(&want_whisper),
        "whisper did not resolve from Resources.\nwanted: {want_whisper}\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&want_sherpa),
        "sherpa did not resolve from Resources/sherpa/bin.\nwanted: {want_sherpa}\nstdout:\n{stdout}"
    );
    assert_eq!(
        got_wsha, want_wsha,
        "vendored whisper-cli sha256 does not match WHISPER_PINS for {target}"
    );
    assert_eq!(
        got_ssha, want_ssha,
        "vendored diarization exe sha256 does not match SHERPA_PINS for {target}"
    );
}

/// The content-hash acceptance proof, and the source of truth `verify-mac-bundle.mjs` defers to
/// for bundle-vs-pin verification. For whisper-cli, the sherpa diarization exe, and the
/// onnxruntime dylib, per macOS target it proves that the RAW vendored sha256 equals the raw
/// provision pin (`sha256` / `binary_sha256`), and that the signing-independent CONTENT hash of a
/// FULLY (ad-hoc) signed copy equals the pin's `content_sha256` — and, for every
/// Developer-ID-signed copy present (the built `.app` in `target/`, and the installed
/// `/Applications/Cleanroom.app` for this host's arch), equals the content hash of the re-signed
/// copy, whose RAW hash DIFFERS from the vendor's.
///
/// That last equality is the whole design: a signed sidecar still matches its pin. "Fully signed"
/// matters for sherpa — its upstream universal2 x86_64 slice is unsigned, but codesign signs it
/// at packaging time, so the pin is a fully-signed reference. Skips cleanly with nothing to prove.
#[cfg(target_os = "macos")]
#[test]
fn signed_bundle_content_hash_matches_the_pins() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let sherpa = pin("sherpa-pin.json");
    let onnx_field = |target: &str, field: &str| -> String {
        sherpa["targets"][target]["binary"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["dest"].as_str().unwrap().contains("onnxruntime"))
            .and_then(|m| m[field].as_str())
            .unwrap_or_else(|| panic!("onnxruntime {field} for {target}"))
            .to_string()
    };
    let triple_for = |t: &str| match t {
        "macos-aarch64" => "aarch64-apple-darwin",
        "macos-x86_64" => "x86_64-apple-darwin",
        _ => unreachable!(),
    };

    let mut proved = 0usize;
    for target in ["macos-aarch64", "macos-x86_64"] {
        let triple = triple_for(target);
        let wp = anvil_asr::WHISPER_PINS
            .iter()
            .find(|p| p.target == target)
            .unwrap();
        let sp = anvil_asr::SHERPA_PINS
            .iter()
            .find(|p| p.target == target)
            .unwrap();
        let res = repo.join(format!(
            "target/{triple}/release/bundle/macos/Cleanroom.app/Contents/Resources"
        ));

        // (label, vendor path, raw pin, content pin, bundled .app path)
        let items: Vec<(&str, PathBuf, String, String, PathBuf)> = vec![
            (
                "whisper-cli",
                repo.join(format!("vendor/whisper/{target}/whisper-cli")),
                wp.binary_sha256.to_string(),
                wp.content_sha256.unwrap().to_string(),
                res.join("whisper/whisper-cli"),
            ),
            (
                "sherpa exe",
                repo.join(format!(
                    "vendor/sherpa/{target}/bin/sherpa-onnx-offline-speaker-diarization"
                )),
                sp.binary_sha256.to_string(),
                sp.content_sha256.unwrap().to_string(),
                res.join("sherpa/bin/sherpa-onnx-offline-speaker-diarization"),
            ),
            (
                "onnxruntime",
                repo.join(format!(
                    "vendor/sherpa/{target}/lib/libonnxruntime.1.17.1.dylib"
                )),
                onnx_field(target, "sha256"),
                onnx_field(target, "content_sha256"),
                res.join("sherpa/lib/libonnxruntime.1.17.1.dylib"),
            ),
        ];

        // The built bundle in target/, plus — for this host's arch — the installed
        // /Applications/Cleanroom.app copy (the artifact the shipped-app failure lived in).
        let host_target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        for (name, vendor, raw_pin, content_pin, app) in items {
            if vendor.is_file() {
                assert_eq!(
                    sha256_file(&vendor),
                    raw_pin,
                    "{target} {name}: raw vendored hash must equal the provision pin"
                );
                assert_eq!(
                    fully_signed_content(&vendor),
                    content_pin,
                    "{target} {name}: fully-signed vendor content hash must equal the pin"
                );
                proved += 1;
            }
            let mut signed_copies = vec![app.clone()];
            if target == host_target {
                let rel = app
                    .strip_prefix(&res)
                    .expect("bundled path is under Resources");
                signed_copies.push(
                    PathBuf::from("/Applications/Cleanroom.app/Contents/Resources").join(rel),
                );
            }
            for signed in signed_copies.into_iter().filter(|p| p.is_file()) {
                let bundled = anvil_asr::macho_content_sha256(&signed)
                    .expect("content-hash the bundled sidecar");
                assert_eq!(
                    bundled,
                    content_pin,
                    "{target} {name}: Developer-ID-signed copy at {} must content-hash to the pin",
                    signed.display()
                );
                if vendor.is_file() {
                    assert_ne!(
                        sha256_file(&signed),
                        sha256_file(&vendor),
                        "{target} {name}: the signed copy's RAW hash must differ from the \
                         vendor's — otherwise this proves nothing about signing-independence"
                    );
                }
                proved += 1;
            }
        }
    }
    if proved == 0 {
        eprintln!("skipping: no vendored sidecars and no signed .app to prove against");
    }
}

/// content hash of `src` after ad-hoc signing EVERY slice — the canonical shipped state (the
/// Developer-ID `.app` hashes identically). macOS-only; `codesign` is always present there.
#[cfg(target_os = "macos")]
fn fully_signed_content(src: &std::path::Path) -> String {
    let dir =
        std::env::temp_dir().join(format!("anvil-asr-sign-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dst = dir.join(src.file_name().expect("file name"));
    std::fs::copy(src, &dst).expect("copy for signing");
    let out = Command::new("codesign")
        .args(["-f", "-s", "-"])
        .arg(&dst)
        .output()
        .expect("run codesign");
    assert!(
        out.status.success(),
        "codesign -f -s - failed on {}: {}",
        dst.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let content = anvil_asr::macho_content_sha256(&dst).expect("content hash the signed copy");
    let _ = std::fs::remove_dir_all(&dir);
    content
}

/// sha256 of a file, lowercase hex, via macOS `shasum` (this helper is only reached on macOS,
/// where `shasum` is always present — so it needs no crate dependency).
fn sha256_file(path: &std::path::Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .expect("run shasum");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("shasum printed a hash")
        .to_string()
}

// --- pin/catalog consistency (always run, no provisioning needed) --------------------------

fn pin(name: &str) -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts")
        .join(name);
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// The sherpa pin ships the diarization *models*; their hashes must equal the app's own catalog
/// (`KNOWN_DIARIZATION_MODELS`), or the provisioner would install a file the app then rejects
/// (or, worse, silently diarizes with an unaudited model).
#[test]
fn sherpa_pin_models_match_the_catalog() {
    use anvil_asr::model::KNOWN_DIARIZATION_MODELS;
    let json = pin("sherpa-pin.json");
    let models = json["models"].as_array().expect("sherpa-pin models[]");
    assert!(
        !models.is_empty(),
        "sherpa pin must carry the default models"
    );

    for m in models {
        let dest = m["dest"].as_str().expect("model dest");
        let sha = m["onnx_sha256"].as_str().expect("model onnx_sha256");
        let bytes = m["onnx_bytes"].as_u64().expect("model onnx_bytes");
        let pack = KNOWN_DIARIZATION_MODELS
            .iter()
            .find(|p| p.filename == dest)
            .unwrap_or_else(|| panic!("no catalog model with filename {dest}"));
        assert_eq!(
            sha, pack.sha256,
            "sha256 for {dest} drifted between sherpa-pin.json and model.rs"
        );
        assert_eq!(
            bytes, pack.size_bytes,
            "size for {dest} drifted between sherpa-pin.json and model.rs"
        );
    }
}

/// Both pins are now target-keyed (`.targets.<os>-<arch>`); every target must be well-formed the
/// same way the ffmpeg pin is: 64-hex lowercase sha256s for every artifact we execute or extract,
/// and (for the download targets) an https source URL. All three targets — the preserved
/// `windows-x86_64` plus the two mac arches — are checked.
#[test]
fn whisper_and_sherpa_pins_are_well_formed() {
    // whisper: each target ships its whisper-cli and 64-hex hashes for the whole member set.
    let w = pin("whisper-pin.json");
    let wt = w["targets"].as_object().expect("whisper-pin targets{}");
    for target in ["windows-x86_64", "macos-aarch64", "macos-x86_64"] {
        let is_mac = target.starts_with("macos-");
        let t = wt
            .get(target)
            .unwrap_or_else(|| panic!("whisper pin missing target {target}"));
        assert!(
            is_sha256(t["binary_sha256"].as_str().unwrap()),
            "{target} binary_sha256"
        );
        // macOS carries the signing-independent content hash (the bundle-vs-pin gate); Windows
        // stays raw only.
        assert_content_sha256(&t["content_sha256"], is_mac, &format!("{target} binary"));
        let cli = if target == "windows-x86_64" {
            "whisper-cli.exe"
        } else {
            "whisper-cli"
        };
        let members = t["members"]
            .as_array()
            .unwrap_or_else(|| panic!("{target} members[]"));
        assert!(
            members.iter().any(|m| m["dest"] == cli),
            "{target} must ship {cli}"
        );
        for m in members {
            assert!(
                is_sha256(m["sha256"].as_str().unwrap()),
                "{target}: bad sha256 for {}",
                m["dest"]
            );
            assert_content_sha256(
                &m["content_sha256"],
                is_mac,
                &format!("{target} {}", m["dest"]),
            );
        }
    }

    // sherpa: each target's binary block has an https source, 64-hex archive + binary hashes, and
    // members shipping the diarization exe + an onnxruntime runtime.
    let s = pin("sherpa-pin.json");
    let st = s["targets"].as_object().expect("sherpa-pin targets{}");
    for target in ["windows-x86_64", "macos-aarch64", "macos-x86_64"] {
        let is_mac = target.starts_with("macos-");
        let b = &st
            .get(target)
            .unwrap_or_else(|| panic!("sherpa pin missing target {target}"))["binary"];
        assert!(
            b["source_url"].as_str().unwrap().starts_with("https://"),
            "{target} source_url"
        );
        assert!(
            is_sha256(b["archive_sha256"].as_str().unwrap()),
            "{target} archive_sha256"
        );
        assert!(
            is_sha256(b["binary_sha256"].as_str().unwrap()),
            "{target} binary_sha256"
        );
        assert_content_sha256(&b["content_sha256"], is_mac, &format!("{target} binary"));
        let members = b["members"]
            .as_array()
            .unwrap_or_else(|| panic!("{target} members[]"));
        assert!(
            members.iter().any(|m| m["dest"]
                .as_str()
                .unwrap()
                .contains("sherpa-onnx-offline-speaker-diarization")),
            "{target} must ship the diarization exe"
        );
        assert!(
            members
                .iter()
                .any(|m| m["dest"].as_str().unwrap().contains("onnxruntime")),
            "{target} must ship an onnxruntime runtime"
        );
        for m in members {
            assert!(
                is_sha256(m["sha256"].as_str().unwrap()),
                "{target}: bad sha256 for {}",
                m["dest"]
            );
            assert_content_sha256(
                &m["content_sha256"],
                is_mac,
                &format!("{target} {}", m["dest"]),
            );
        }
    }
}

/// A macOS pin field must carry a well-formed content hash; a non-mac one must carry none.
fn assert_content_sha256(v: &serde_json::Value, is_mac: bool, who: &str) {
    match v.as_str() {
        Some(c) => {
            assert!(is_mac, "{who}: content_sha256 present on a non-macOS entry");
            assert!(
                is_sha256(c),
                "{who}: content_sha256 not lowercase-hex sha256"
            );
        }
        None => assert!(!is_mac, "{who}: macOS entry is missing content_sha256"),
    }
}

/// Schema guardian across ALL targets: the code-side pin tables ([`anvil_asr::WHISPER_PINS`] /
/// [`anvil_asr::SHERPA_PINS`]) must match the JSON provisioning pins, or the fetch/build scripts
/// would stage a binary the app's own pin table disagrees with. This is anvil-asr's analogue of
/// anvil_media's `pin_json_matches_the_code`, extended to every target (windows + both mac arches).
#[test]
fn pin_json_matches_the_code() {
    use anvil_asr::{SHERPA_PINS, WHISPER_PINS};

    fn same_target_set(
        code: &[&str],
        json: &serde_json::Map<String, serde_json::Value>,
        who: &str,
    ) {
        let mut c: Vec<&str> = code.to_vec();
        c.sort_unstable();
        let mut j: Vec<&str> = json.keys().map(String::as_str).collect();
        j.sort_unstable();
        assert_eq!(c, j, "{who}: code pin targets != JSON pin targets");
    }

    let w = pin("whisper-pin.json");
    let wt = w["targets"].as_object().expect("whisper targets{}");
    same_target_set(
        &WHISPER_PINS.iter().map(|p| p.target).collect::<Vec<_>>(),
        wt,
        "whisper",
    );
    for p in WHISPER_PINS {
        let t = &wt[p.target];
        assert_eq!(
            t["version"].as_str(),
            Some(p.version),
            "{} version",
            p.target
        );
        assert_eq!(
            t["binary_sha256"].as_str(),
            Some(p.binary_sha256),
            "{} binary_sha256 drift between whisper-pin.json and WHISPER_PINS",
            p.target
        );
        assert_eq!(
            t["content_sha256"].as_str(),
            p.content_sha256,
            "{} content_sha256 drift between whisper-pin.json and WHISPER_PINS",
            p.target
        );
        assert_eq!(
            t["license"].as_str(),
            Some(p.license),
            "{} license",
            p.target
        );
    }

    let s = pin("sherpa-pin.json");
    let st = s["targets"].as_object().expect("sherpa targets{}");
    same_target_set(
        &SHERPA_PINS.iter().map(|p| p.target).collect::<Vec<_>>(),
        st,
        "sherpa",
    );
    for p in SHERPA_PINS {
        // sherpa's per-target binary lives under `.binary` (a block, since it carries the archive
        // + member manifest); the code pin mirrors its version + primary-binary hash + licence.
        let t = &st[p.target];
        assert_eq!(
            t["version"].as_str(),
            Some(p.version),
            "{} version",
            p.target
        );
        assert_eq!(
            t["binary"]["binary_sha256"].as_str(),
            Some(p.binary_sha256),
            "{} binary_sha256 drift between sherpa-pin.json and SHERPA_PINS",
            p.target
        );
        assert_eq!(
            t["binary"]["content_sha256"].as_str(),
            p.content_sha256,
            "{} content_sha256 drift between sherpa-pin.json and SHERPA_PINS",
            p.target
        );
        assert_eq!(
            t["binary"]["license"].as_str(),
            Some(p.license),
            "{} license",
            p.target
        );
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
