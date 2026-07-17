//! Model manager for the ASR lane — whisper.cpp ggml packs **and** the diarization ONNX packs.
//!
//! Enumerates the known whisper.cpp model packs (tiny/base/small/medium/large-v3-turbo, in
//! English-only `.en` and multilingual variants) and the ONNX packs the diarization sidecar
//! needs (segmentation + speaker embedding), and reports which are **installed** — i.e.
//! present as a file in the models directory.
//!
//! Airplane-mode (ADR-005 engine invariant): this module **never downloads** anything. It
//! only lists what *could* exist (`url` + checksum, for a UI/installer to fetch **once**, up
//! front, as a deliberate user action) and locates what *does* exist on disk. Nothing in the
//! transcribe or diarize path ever touches the network.
//!
//! ## Checksums: one story with the desktop
//! The whisper.cpp project publishes **sha1** (not sha256) for its ggml weights, in its own
//! `models/README.md`. `apps/desktop/src-tauri/src/models.rs` already verifies downloads
//! against those exact sha1 values. This catalog carries the **same** url + sha1, so the two
//! agree by construction — the desktop manifest mirrors this catalog rather than telling a
//! second story (its `asr_ggml_filename` already resolves filenames *through* `known_models`).
//! The whisper `url` is the canonical Hugging Face `resolve/main` path; `main` is safe because
//! the sha1 pin fails the download closed if a file ever changes. (The diarization ONNX packs
//! below are a *different* upstream — sherpa-onnx — which publishes sha256, so they pin by
//! sha256. Two checksum algorithms, but one story each: whatever the upstream publishes.)
//!
//! ## Models directory resolution
//! [`models_dirs`] returns the search path, first match wins:
//! 1. `CLEANROOM_WHISPER_MODELS_DIR` environment variable (explicit dir override — always wins),
//! 2. the per-user **config** models dir (`~/Library/Application Support/anvil/models` on
//!    macOS, `%APPDATA%\anvil\models` on Windows — via `anvil_core`'s platform abstraction, the
//!    same `dirs` crate the rest of the app uses). This is where `anvil-models` / the desktop
//!    Models screen and `anvil models pull` actually **download** weights, so an installed pack
//!    is found here before anything bundled. It is always user-writable and, on macOS,
//!    deliberately **not** the signed `.app` bundle (which is unwritable without admin rights
//!    and whose code signature a write would break) — the reason a "download next to the exe"
//!    design has no viable Mac equivalent and the config dir is used on every platform.
//! 3. a `models/` folder next to the current executable (bundled/portable layout),
//! 4. a `models/` folder under `../Resources/` (the macOS `.app` layout — the exe is in
//!    `Contents/MacOS/` and the installer drops the bundled diarization models in
//!    `Contents/Resources/models`),
//! 5. a `models/` folder in the current working directory (dev layout).

use std::io::Read;
use std::path::{Path, PathBuf};

use anvil_core::platform::Platform;
use sha2::{Digest, Sha256};

use crate::error::AsrError;

/// A known whisper.cpp ggml model pack. `size_bytes` is the approximate download size of the
/// F16 build — enough for a UI to show "≈466 MB" and gate on free disk, not an integrity
/// check (that is [`ModelPack::sha1`], verified once after download). Transcription itself
/// verifies nothing about the model beyond that the file exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPack {
    /// Stable id, also the infix of the filename: `tiny.en`, `small`, `large-v3-turbo`, …
    pub id: &'static str,
    /// Human label for the UI.
    pub display_name: &'static str,
    /// ggml filename this pack installs as, e.g. `ggml-small.en.bin`.
    pub filename: &'static str,
    /// `true` for multilingual models, `false` for the English-only `.en` variants.
    pub multilingual: bool,
    /// Approximate on-disk size in bytes (F16 build).
    pub size_bytes: u64,
    /// SPDX-ish licence of the weights, for the attribution screen. MIT for every ggml pack.
    pub license: &'static str,
    /// Where a UI/installer fetches this **once**, up front. The canonical Hugging Face
    /// `resolve/main` URL for the ggml weight. Never fetched by the transcribe path.
    pub url: &'static str,
    /// Pinned **sha1** of the ggml file, lowercase hex, copied from whisper.cpp's
    /// `models/README.md`. sha1 (not sha256) because that is what whisper.cpp publishes; see
    /// the module docs and [`verify_ggml`].
    pub sha1: &'static str,
}

const MIB: u64 = 1024 * 1024;

/// Base URL for the canonical whisper.cpp ggml weights on Hugging Face (MIT). Same host the
/// desktop models manager downloads from, so the two never diverge on where a pack comes from.
/// Test-only: the catalog stores full-literal URLs (clearer at the entry), and the well-formed
/// test asserts each begins with this host.
#[cfg(test)]
const GGML_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

/// The catalog of model packs Cleanroom knows how to use. English-only `.en` variants transcribe
/// English faster/more accurately; the multilingual variants are required for `-l auto` on
/// non-English audio. `large-v3-turbo` is multilingual-only (there is no `.en` turbo build).
pub const KNOWN_MODELS: &[ModelPack] = &[
    ModelPack {
        id: "tiny.en",
        display_name: "Tiny (English)",
        filename: "ggml-tiny.en.bin",
        multilingual: false,
        size_bytes: 75 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
        sha1: "c78c86eb1a8faa21b369bcd33207cc90d64ae9df",
    },
    ModelPack {
        id: "tiny",
        display_name: "Tiny (multilingual)",
        filename: "ggml-tiny.bin",
        multilingual: true,
        size_bytes: 75 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        sha1: "bd577a113a864445d4c299885e0cb97d4ba92b5f",
    },
    ModelPack {
        id: "base.en",
        display_name: "Base (English)",
        filename: "ggml-base.en.bin",
        multilingual: false,
        size_bytes: 142 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
        sha1: "137c40403d78fd54d454da0f9bd998f78703390c",
    },
    ModelPack {
        id: "base",
        display_name: "Base (multilingual)",
        filename: "ggml-base.bin",
        multilingual: true,
        size_bytes: 142 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha1: "465707469ff3a37a2b9b8d8f89f2f99de7299dac",
    },
    ModelPack {
        id: "small.en",
        display_name: "Small (English)",
        filename: "ggml-small.en.bin",
        multilingual: false,
        size_bytes: 466 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
        sha1: "db8a495a91d927739e50b3fc1cc4c6b8f6c2d022",
    },
    ModelPack {
        id: "small",
        display_name: "Small (multilingual)",
        filename: "ggml-small.bin",
        multilingual: true,
        size_bytes: 466 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha1: "55356645c2b361a969dfd0ef2c5a50d530afd8d5",
    },
    ModelPack {
        id: "medium.en",
        display_name: "Medium (English)",
        filename: "ggml-medium.en.bin",
        multilingual: false,
        size_bytes: 1536 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.en.bin",
        sha1: "8c30f0e44ce9560643ebd10bbe50cd20eafd3723",
    },
    ModelPack {
        id: "medium",
        display_name: "Medium (multilingual)",
        filename: "ggml-medium.bin",
        multilingual: true,
        size_bytes: 1536 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha1: "fd9727b6e1217c2f614f9b698455c4ffd82463b4",
    },
    ModelPack {
        id: "large-v3-turbo",
        display_name: "Large v3 Turbo (multilingual)",
        filename: "ggml-large-v3-turbo.bin",
        multilingual: true,
        size_bytes: 1620 * MIB,
        license: WHISPER_LICENSE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
        sha1: "4af2b29d7ec73d781377bfd1758ca957a807e941",
    },
];

/// Every whisper.cpp ggml weight is MIT (per handoff/07-RISKS-LEGAL §1, "Whisper weights ✔").
const WHISPER_LICENSE: &str = "MIT (whisper.cpp ggml weights)";

/// A model pack found on disk, paired with the concrete path to its `.bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModel {
    /// The catalog entry this file corresponds to.
    pub pack: ModelPack,
    /// Absolute (or dir-relative) path to the installed `ggml-*.bin`.
    pub path: PathBuf,
}

/// The full model catalog (same as [`KNOWN_MODELS`], exposed as a function for symmetry with
/// [`installed_models`]).
pub fn known_models() -> &'static [ModelPack] {
    KNOWN_MODELS
}

/// The models-directory search path (see the module docs). Only directories that exist are
/// returned, in priority order; the list may be empty if none exist yet.
pub fn models_dirs() -> Vec<PathBuf> {
    let mut out = candidate_models_dirs();
    out.retain(|d| d.is_dir());
    out
}

/// The ordered candidate model directories *before* existence filtering (see [`models_dirs`]
/// for the resolution story). Split out so the search order is unit-testable without a
/// populated filesystem.
fn candidate_models_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(explicit) = std::env::var_os("CLEANROOM_WHISPER_MODELS_DIR") {
        out.push(PathBuf::from(explicit));
    }
    // The per-user config models dir — where the sanctioned downloader (`anvil-models`, used by
    // both the desktop Models screen and `anvil models pull`) writes verified packs. Searched
    // before anything bundled so a downloaded pack is found first, and always user-writable
    // (never the signed `.app` bundle — see the module docs). Resolved through `anvil_core`'s
    // platform abstraction (the same `dirs` crate the rest of the app uses), so it stays
    // cross-platform with no `#[cfg]` here; the `is_dir` retain in `models_dirs` drops it until
    // it exists.
    out.push(anvil_core::platform::current().config_dir().join("models"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("models"));
            // macOS `.app`: `bundle.resources` (the `models/` folder) land in
            // `Contents/Resources/`, i.e. `../Resources/models` relative to the exe in
            // `Contents/MacOS/`. Added unconditionally (no `#[cfg]`); it does not exist in a
            // Windows/flat install and is dropped by the `is_dir` retain, so non-mac
            // resolution is unchanged.
            out.push(dir.join("../Resources/models"));
        }
    }
    out.push(PathBuf::from("models"));
    out
}

/// Every known model that is present in one of the [`models_dirs`]. A given `id` is reported
/// at most once (the first directory that has it wins), preserving [`KNOWN_MODELS`] order.
pub fn installed_models() -> Vec<InstalledModel> {
    let dirs = models_dirs();
    let mut out = Vec::new();
    for pack in KNOWN_MODELS {
        if let Some(path) = dirs
            .iter()
            .map(|d| d.join(pack.filename))
            .find(|p| p.is_file())
        {
            out.push(InstalledModel { pack: *pack, path });
        }
    }
    out
}

/// Locate an installed model's `.bin` by catalog `id` (e.g. `"small.en"`). Returns `None` if
/// the id is unknown or the file is not present in any models dir. Never downloads.
pub fn locate_model(id: &str) -> Option<PathBuf> {
    let pack = KNOWN_MODELS.iter().find(|m| m.id == id)?;
    models_dirs()
        .iter()
        .map(|d| d.join(pack.filename))
        .find(|p| p.is_file())
}

// --- diarization packs -------------------------------------------------------------------

/// Which stage of the diarization pipeline a [`DiarModelPack`] feeds.
///
/// The sidecar (see [`crate::diarize`]) runs the ADR-004 pipeline —
/// *segmentation → speaker embeddings → clustering* — and needs exactly one model of each
/// kind. Clustering is pure math and needs no model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiarModelKind {
    /// Local speaker-activity segmentation (a pyannote-style powerset model).
    Segmentation,
    /// Speaker-embedding extractor, whose vectors get clustered into speakers.
    Embedding,
}

/// A known diarization ONNX model. Unlike [`ModelPack`] these are hash-verified:
/// [`verify_model`] checks `sha256` before the sidecar is ever pointed at the file, so a
/// truncated or swapped download fails loudly instead of quietly diarizing badly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiarModelPack {
    /// Stable id used to select the pack, e.g. `"pyannote-segmentation-3.0"`.
    pub id: &'static str,
    /// Human label for the UI.
    pub display_name: &'static str,
    /// Filename this pack installs as, inside a [`models_dirs`] entry.
    pub filename: &'static str,
    /// Which pipeline stage this model serves.
    pub kind: DiarModelKind,
    /// Exact on-disk size in bytes.
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: &'static str,
    /// SPDX-ish licence string, for the attribution screen (RISKS-LEGAL §6).
    pub license: &'static str,
    /// Where a UI/installer fetches this **once**, up front. Never fetched by [`crate::diarize`].
    pub download_url: &'static str,
    /// `true` for the pack Cleanroom selects when the caller does not name one.
    pub default: bool,
}

/// The diarization model catalog: one segmentation model + the speaker-embedding extractors.
///
/// **Why TitaNet-small is the default embedding model and not WeSpeaker CAM++** (which
/// ADR-004 named): CAM++ — both the WeSpeaker and the 3D-Speaker build — emits degenerate
/// embeddings under the prebuilt Windows x64 sherpa-onnx / ONNX Runtime combination this
/// sidecar ships. sherpa-onnx drops NaN embedding rows, and enough of them are dropped that
/// clustering is left with a single row and labels *the entire file* as one speaker,
/// regardless of `--clustering.num-clusters`. It was reproduced on our own fixture and on
/// sherpa-onnx's own `2-two-speakers-en.wav`. NeMo TitaNet-small runs the same files
/// correctly. CAM++ stays in the catalog (it is smaller, and is fine on the platforms where
/// it works) but it is not the default and is not what the DER gate is measured on.
pub const KNOWN_DIARIZATION_MODELS: &[DiarModelPack] = &[
    DiarModelPack {
        id: "pyannote-segmentation-3.0",
        display_name: "pyannote segmentation 3.0",
        filename: "sherpa-onnx-pyannote-segmentation-3-0.onnx",
        kind: DiarModelKind::Segmentation,
        size_bytes: 5_992_913,
        sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
        license: "MIT (pyannote/segmentation-3.0, © 2022 CNRS)",
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2",
        default: true,
    },
    DiarModelPack {
        id: "titanet-small",
        display_name: "NVIDIA NeMo TitaNet-small (English)",
        filename: "nemo_en_titanet_small.onnx",
        kind: DiarModelKind::Embedding,
        size_bytes: 40_257_283,
        sha256: "ad4a1802485d8b34c722d2a9d04249662f2ece5d28a7a039063ca22f515a789e",
        license: "CC-BY-4.0 (NVIDIA NeMo — attribution required)",
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/nemo_en_titanet_small.onnx",
        default: true,
    },
    DiarModelPack {
        id: "wespeaker-campplus",
        display_name: "WeSpeaker CAM++ (English) — degenerate on Windows, see docs",
        filename: "wespeaker_en_voxceleb_CAM++.onnx",
        kind: DiarModelKind::Embedding,
        size_bytes: 29_292_684,
        sha256: "c46fad10b5f81e1aa4a60c162714208577093655076c5450f8c469e522ec54ef",
        license: "Apache-2.0 (WeSpeaker)",
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_CAM%2B%2B.onnx",
        default: false,
    },
];

/// The full diarization catalog (see [`KNOWN_DIARIZATION_MODELS`]).
pub fn known_diarization_models() -> &'static [DiarModelPack] {
    KNOWN_DIARIZATION_MODELS
}

/// Every known diarization model present in one of the [`models_dirs`], in catalog order.
pub fn installed_diarization_models() -> Vec<(DiarModelPack, PathBuf)> {
    let dirs = models_dirs();
    let mut out = Vec::new();
    for pack in KNOWN_DIARIZATION_MODELS {
        if let Some(path) = dirs
            .iter()
            .map(|d| d.join(pack.filename))
            .find(|p| p.is_file())
        {
            out.push((*pack, path));
        }
    }
    out
}

/// Locate an installed diarization model by catalog `id`. Returns `None` if the id is unknown
/// or the file is not on disk. Never downloads.
pub fn locate_diarization_model(id: &str) -> Option<PathBuf> {
    let pack = KNOWN_DIARIZATION_MODELS.iter().find(|m| m.id == id)?;
    models_dirs()
        .iter()
        .map(|d| d.join(pack.filename))
        .find(|p| p.is_file())
}

/// The first installed model of `kind`, preferring the catalog's `default` pack.
pub(crate) fn default_diarization_model(kind: DiarModelKind) -> Option<PathBuf> {
    let installed = installed_diarization_models();
    installed
        .iter()
        .find(|(p, _)| p.kind == kind && p.default)
        .or_else(|| installed.iter().find(|(p, _)| p.kind == kind))
        .map(|(_, path)| path.clone())
}

/// Hash-verify a downloaded diarization model against its catalog entry.
///
/// Streams the file in 1 MiB chunks (these run to tens of MB) and compares size then SHA-256.
/// Call this **once, after downloading** — never on the inference path, where it would add a
/// pointless full re-read of the file to every job.
pub fn verify_model(path: &Path, pack: &DiarModelPack) -> Result<(), AsrError> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len != pack.size_bytes {
        return Err(AsrError::ModelCorrupt(format!(
            "{}: expected {} bytes, found {len}",
            path.display(),
            pack.size_bytes
        )));
    }

    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex_lower(&hasher.finalize());
    if got != pack.sha256 {
        return Err(AsrError::ModelCorrupt(format!(
            "{}: sha256 mismatch (expected {}, got {got})",
            path.display(),
            pack.sha256
        )));
    }
    Ok(())
}

/// Lowercase hex of a byte slice (no dependency on a hex crate for 32 bytes).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique_and_well_formed() {
        let mut ids: Vec<&str> = KNOWN_MODELS.iter().map(|m| m.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "model ids must be unique");

        for pack in KNOWN_MODELS {
            assert!(pack.filename.starts_with("ggml-"));
            assert!(pack.filename.ends_with(".bin"));
            // `.en` id <=> English-only <=> not multilingual.
            assert_eq!(pack.id.ends_with(".en"), !pack.multilingual, "{}", pack.id);
        }
    }

    /// The release blocker for the ASR lane: every whisper pack is pinned — a real
    /// canonical-host URL and a well-formed sha1 (whisper.cpp's own published checksum), so the
    /// models manager can verify what it downloads. Mirrors the desktop manifest's values (see
    /// the module docs) rather than telling a second story.
    #[test]
    fn every_whisper_pack_is_pinned_and_well_formed() {
        for pack in KNOWN_MODELS {
            assert!(
                pack.url.starts_with(GGML_BASE_URL),
                "{} url must be on the canonical ggml host: {}",
                pack.id,
                pack.url
            );
            assert!(
                pack.url.ends_with(pack.filename),
                "{}: url should resolve to its filename ({} vs {})",
                pack.id,
                pack.url,
                pack.filename
            );
            assert_eq!(
                pack.sha1.len(),
                40,
                "{} sha1 must be 20 bytes (whisper.cpp publishes sha1)",
                pack.id
            );
            assert!(
                pack.sha1
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{} sha1 must be lowercase hex",
                pack.id
            );
            assert!(!pack.license.is_empty(), "{} needs a licence", pack.id);
        }
    }

    #[test]
    fn locate_model_rejects_unknown_id() {
        assert!(locate_model("does-not-exist").is_none());
    }

    #[test]
    fn per_user_config_dir_is_searched_before_the_app_bundle() {
        // A pack downloaded into the per-user config models dir (where `anvil-models` /
        // `anvil models pull` write) must be found *before* anything staged in the macOS `.app`
        // bundle's `Resources/models`, so a fresh download always wins over a stale bundle copy
        // — and, being user-writable, it's the mac-safe destination the whole download flow
        // depends on. This checks the candidate ORDER (pre-existence-filter) so it's hermetic.
        let cands = candidate_models_dirs();
        let config = anvil_core::platform::current().config_dir().join("models");
        let cfg_idx = cands
            .iter()
            .position(|d| *d == config)
            .expect("the per-user config models dir must be a candidate");
        if let Some(bundle_idx) = cands.iter().position(|d| d.ends_with("Resources/models")) {
            assert!(
                cfg_idx < bundle_idx,
                "config dir ({cfg_idx}) must be searched before the .app bundle ({bundle_idx})"
            );
        }
        // With no explicit `CLEANROOM_WHISPER_MODELS_DIR` override in this test process, the config
        // dir is the very first place searched.
        if std::env::var_os("CLEANROOM_WHISPER_MODELS_DIR").is_none() {
            assert_eq!(cands.first(), Some(&config));
        }
    }

    #[test]
    fn diarization_catalog_is_well_formed() {
        let mut ids: Vec<&str> = KNOWN_DIARIZATION_MODELS.iter().map(|m| m.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "diarization model ids must be unique");

        for pack in KNOWN_DIARIZATION_MODELS {
            assert!(pack.filename.ends_with(".onnx"), "{}", pack.id);
            assert!(pack.size_bytes > 0, "{}", pack.id);
            assert_eq!(pack.sha256.len(), 64, "{} sha256 must be 32 bytes", pack.id);
            assert!(
                pack.sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{} sha256 must be lowercase hex",
                pack.id
            );
            assert!(!pack.license.is_empty(), "{} needs a licence", pack.id);
            assert!(
                pack.download_url.starts_with("https://"),
                "{} download_url must be https",
                pack.id
            );
        }
    }

    /// The pipeline needs exactly one default of each kind, or resolution is ambiguous.
    #[test]
    fn diarization_catalog_has_one_default_per_kind() {
        for kind in [DiarModelKind::Segmentation, DiarModelKind::Embedding] {
            let defaults = KNOWN_DIARIZATION_MODELS
                .iter()
                .filter(|p| p.kind == kind && p.default)
                .count();
            assert_eq!(defaults, 1, "{kind:?} must have exactly one default pack");
        }
    }

    #[test]
    fn locate_diarization_model_rejects_unknown_id() {
        assert!(locate_diarization_model("does-not-exist").is_none());
    }

    #[test]
    fn verify_model_rejects_wrong_size() {
        let dir = std::env::temp_dir().join(format!("anvil-asr-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("fake.onnx");
        std::fs::write(&path, b"not a model").expect("write");

        let pack = KNOWN_DIARIZATION_MODELS[0];
        let err = verify_model(&path, &pack).expect_err("a 11-byte file is not a 6 MB model");
        assert!(matches!(err, AsrError::ModelCorrupt(_)), "{err:?}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hex_lower_pads_bytes() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa1, 0xff]), "000fa1ff");
    }
}
