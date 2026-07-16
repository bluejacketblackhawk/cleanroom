//! Build-time provisioning of the DeepFilterNet3 model (03 §4.4 "Standard: DeepFilterNet3").
//!
//! The Standard tier is the one-click default, so its model must always be there — no
//! "download a pack first" step, no runtime fetch (ADR-004 airplane-mode). We therefore
//! bake the model into the binary: this script puts `DeepFilterNet3_onnx.tar.gz` (three
//! ONNX graphs + `config.ini`, ~8 MB, MIT/Apache-2.0 from the DeepFilterNet project) in
//! `OUT_DIR`, and `dfn3.rs` `include_bytes!`s it. At run time there is no network call and
//! no model file to find.
//!
//! Resolution order (first hit wins), all hash-verified against [`SHA256`]:
//!   1. `ANVIL_DFN3_TARBALL` — explicit path to a local copy (offline / air-gapped builds).
//!   2. `crates/anvil-ai/models/DeepFilterNet3_onnx.tar.gz` — a vendored copy, if present.
//!   3. `OUT_DIR/DeepFilterNet3_onnx.tar.gz` — the cache from a previous build.
//!   4. Download once from the pinned upstream URL, then cache in `OUT_DIR`.
//!
//! A hash mismatch is a hard build error: we never compile against a model we did not pin.

use std::path::{Path, PathBuf};
use std::{env, fs};

use sha2::{Digest, Sha256};

/// Pinned upstream artifact. `v0.5.6` is the last tagged DeepFilterNet release; the tag
/// (not `main`) is what makes this reproducible.
const URL: &str =
    "https://github.com/Rikorose/DeepFilterNet/raw/v0.5.6/models/DeepFilterNet3_onnx.tar.gz";
/// sha256 of `DeepFilterNet3_onnx.tar.gz` (7_983_136 bytes).
const SHA256: &str = "c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616";
const FILENAME: &str = "DeepFilterNet3_onnx.tar.gz";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ANVIL_DFN3_TARBALL");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is always set by cargo"));
    let cached = out_dir.join(FILENAME);

    // 1. Explicit local override (air-gapped builds).
    if let Some(explicit) = env::var_os("ANVIL_DFN3_TARBALL") {
        let path = PathBuf::from(explicit);
        let bytes = fs::read(&path)
            .unwrap_or_else(|e| panic!("ANVIL_DFN3_TARBALL={} unreadable: {e}", path.display()));
        verify(&bytes, &path.display().to_string());
        fs::write(&cached, &bytes).expect("write model cache");
        return;
    }

    // 2. Vendored copy inside the crate.
    let vendored = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("models")
        .join(FILENAME);
    if vendored.is_file() {
        let bytes = fs::read(&vendored).expect("read vendored model");
        verify(&bytes, &vendored.display().to_string());
        fs::write(&cached, &bytes).expect("write model cache");
        return;
    }

    // 3. Previous build's cache.
    if cached.is_file() {
        let bytes = fs::read(&cached).expect("read cached model");
        if hex_digest(&bytes) == SHA256 {
            return;
        }
        // Corrupt/stale cache: fall through and re-download.
        let _ = fs::remove_file(&cached);
    }

    // 4. Fetch once, verify, cache.
    let bytes = download();
    verify(&bytes, URL);
    fs::write(&cached, &bytes).expect("write model cache");
}

fn download() -> Vec<u8> {
    let mut resp = ureq::get(URL).call().unwrap_or_else(|e| {
        panic!(
            "could not fetch the DeepFilterNet3 model from {URL}: {e}\n\
             Offline build? Download it once and point ANVIL_DFN3_TARBALL at the file, or \
             drop it in crates/anvil-ai/models/{FILENAME}."
        )
    });
    let mut bytes = Vec::new();
    use std::io::Read;
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .expect("read model body");
    bytes
}

fn verify(bytes: &[u8], source: &str) {
    let got = hex_digest(bytes);
    assert!(
        got == SHA256,
        "DeepFilterNet3 model hash mismatch from {source}\n  expected {SHA256}\n  got      {got}"
    );
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
