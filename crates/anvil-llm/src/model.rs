//! gguf model-pack manager (ADR-004 §Model packs).
//!
//! Enumerates the Qwen2.5-Instruct packs Cleanroom knows how to run and reports which are
//! **installed** — i.e. present as their `.gguf` file(s) in the models directory. Mirrors
//! [`anvil_asr::model`], plus the sha256 verification the LLM packs need because they are
//! multi-GB downloads that can silently truncate.
//!
//! ## License (this is not a detail — see 07-RISKS-LEGAL)
//! Only **Apache-2.0** Qwen2.5 sizes may ship: 7B-Instruct (default) and 1.5B-Instruct
//! (low-RAM). **Qwen2.5-3B is under a research license and must never be added here** — the
//! catalog test enforces that no pack id contains `3b`.
//!
//! ## Provisioning (downloaded once, never at inference time)
//! This module **never downloads** anything — same airplane-mode invariant as
//! [`anvil_asr::model`] and the ffmpeg sidecar. The models manager (S7) fetches a pack once,
//! user-initiated, from the release asset(s) in each [`ModelFile::url`], writes them into the
//! models dir, and calls [`verify_model`] before marking it installed. Inference only ever
//! *locates* an already-installed, already-verified file.
//!
//! Both shipping packs are pinned to their **canonical upstream** Qwen GGUF repos on Hugging
//! Face (Apache-2.0), by real sha256. We pin upstream directly — the same posture
//! `apps/desktop`'s models manager already uses for the whisper weights — rather than blocking
//! the release on first mirroring the files to an Cleanroom GitHub Release (the org/repo name is
//! still an open owner decision, STATE.md). Mirroring later is a pure URL swap: the sha256
//! pins do not change, so the integrity story is identical whoever hosts the bytes.
//!
//! ## Split ggufs
//! Qwen publishes the 7B q4_k_m quant as **two shards** (`…-00001-of-00002.gguf` +
//! `…-00002-of-00002.gguf`), so a pack is a *list* of [`ModelFile`]s, not one file. llama.cpp
//! auto-loads the sibling shards when `-m` is pointed at part 1 (it reads the `split.count`
//! GGUF key and expands the `%s-%05d-of-%05d.gguf` pattern), so [`ModelPack::primary_filename`]
//! — what inference passes to `-m` — is always the first file. The 1.5B pack is a single file.
//!
//! ## Models directory resolution
//! [`models_dirs`] returns the search path, first match wins:
//! 1. `CLEANROOM_LLM_MODELS_DIR` environment variable (explicit dir),
//! 2. a `models/` folder next to the current executable (bundled layout),
//! 3. a `models/` folder in the current working directory (dev layout).

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::LlmError;

const MIB: u64 = 1024 * 1024;

/// One concrete gguf file within a [`ModelPack`]. A single-file pack has exactly one; a split
/// upstream gguf has one per shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    /// The filename this file installs as, inside a models dir. For a split pack the first
    /// entry's name is the one llama.cpp's `-m` targets.
    pub filename: &'static str,
    /// Release-asset URL the models manager downloads this file from. Empty = not provisioned
    /// (see [`ModelPack::is_provisioned`]).
    pub url: &'static str,
    /// Pinned sha256 of this file, lowercase hex. Empty = unpinned; [`verify_model`] then
    /// reports [`Verification::Unpinned`] rather than passing silently.
    pub sha256: &'static str,
    /// Exact on-disk size in bytes of this file.
    pub size_bytes: u64,
}

/// A known Qwen2.5-Instruct gguf pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPack {
    /// Stable id used by the UI/CLI, e.g. `qwen2.5-7b-instruct-q4_k_m`.
    pub id: &'static str,
    /// Human label for the models manager.
    pub display_name: &'static str,
    /// SPDX license of the **weights**. Apache-2.0 for every pack we ship, no exceptions.
    pub license: &'static str,
    /// Upstream provenance, shown in the licenses/attribution screen.
    pub source: &'static str,
    /// The gguf file(s) this pack installs. One entry for a single-file quant; one per shard
    /// for a split upstream gguf (see the module docs).
    pub files: &'static [ModelFile],
    /// The model's native context window in tokens. What we actually *request* at run time
    /// is [`crate::GenerateOptions::ctx_tokens`], which is smaller by default to bound RAM.
    pub context_tokens: usize,
    /// Rough RAM needed to run it (weights + KV cache headroom); the UI uses this to
    /// recommend the 1.5B pack on small machines.
    pub min_ram_bytes: u64,
}

impl ModelPack {
    /// Whether **every** file in this pack has a download URL *and* a pinned hash — i.e. the
    /// models manager can fetch and verify the whole pack. Packs that are not provisioned can
    /// still be used by pointing `CLEANROOM_LLM_MODEL` at a locally obtained gguf.
    pub fn is_provisioned(&self) -> bool {
        !self.files.is_empty()
            && self
                .files
                .iter()
                .all(|f| !f.url.is_empty() && !f.sha256.is_empty())
    }

    /// The filename inference passes to llama.cpp's `-m` — the first (or only) file. For a
    /// split pack, llama.cpp loads the remaining shards itself from the sibling files.
    pub fn primary_filename(&self) -> &'static str {
        self.files.first().map(|f| f.filename).unwrap_or("")
    }

    /// Total on-disk/download size of the whole pack — for the "will download X GB" line and
    /// free-disk gating, not an integrity check.
    pub fn size_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size_bytes).sum()
    }
}

/// The default pack (ADR-004): best quality that fits a normal desktop.
pub const DEFAULT_MODEL_ID: &str = "qwen2.5-7b-instruct-q4_k_m";
/// The low-RAM pack (ADR-004): fits 8 GB machines, noticeably weaker shownotes.
pub const LOW_RAM_MODEL_ID: &str = "qwen2.5-1.5b-instruct-q4_k_m";

/// The catalog of gguf packs Cleanroom knows how to run.
///
/// Pinned to the canonical upstream Qwen repos (Apache-2.0) by real sha256 — the LFS object
/// ids Hugging Face publishes, which are the files' sha256 (verified during provisioning).
/// See the module docs for why we pin upstream rather than an Cleanroom-hosted mirror, and how a
/// split gguf is represented.
pub const KNOWN_MODELS: &[ModelPack] = &[
    ModelPack {
        id: DEFAULT_MODEL_ID,
        display_name: "Shownotes — Qwen2.5 7B (recommended)",
        license: "Apache-2.0",
        source: "https://huggingface.co/Qwen/Qwen2.5-7B-Instruct-GGUF",
        // Upstream ships this quant as two shards; llama.cpp loads shard 2 automatically when
        // `-m` points at shard 1. Both are pinned by real sha256.
        files: &[
            ModelFile {
                filename: "qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf",
                url: "https://huggingface.co/Qwen/Qwen2.5-7B-Instruct-GGUF/resolve/main/qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf",
                sha256: "dfce12e3862a5283ccfb88221b48480e58745165de856439950d0f22590580db",
                size_bytes: 3_993_201_344,
            },
            ModelFile {
                filename: "qwen2.5-7b-instruct-q4_k_m-00002-of-00002.gguf",
                url: "https://huggingface.co/Qwen/Qwen2.5-7B-Instruct-GGUF/resolve/main/qwen2.5-7b-instruct-q4_k_m-00002-of-00002.gguf",
                sha256: "539cf93f78e887edea1c04e2d7d8cdaca9d01dae9c9025bcb8accbe29df3d72a",
                size_bytes: 689_872_288,
            },
        ],
        context_tokens: 32_768,
        min_ram_bytes: 8 * 1024 * MIB,
    },
    ModelPack {
        id: LOW_RAM_MODEL_ID,
        display_name: "Shownotes — Qwen2.5 1.5B (low RAM)",
        license: "Apache-2.0",
        source: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF",
        files: &[ModelFile {
            filename: "qwen2.5-1.5b-instruct-q4_k_m.gguf",
            url: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf",
            sha256: "6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e",
            size_bytes: 1_117_320_736,
        }],
        context_tokens: 32_768,
        min_ram_bytes: 3 * 1024 * MIB,
    },
];

/// A pack found on disk, paired with the concrete path to its primary `.gguf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModel {
    /// The catalog entry this file corresponds to.
    pub pack: ModelPack,
    /// Path to the installed primary `.gguf` (what inference passes to `-m`).
    pub path: PathBuf,
}

/// Outcome of a [`verify_model`] integrity check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// Every file's sha256 matches the catalog's pinned hash.
    Verified,
    /// The catalog has no pinned hash for (at least one file of) this pack. The file(s) were
    /// hashed but nothing could be compared; the caller should warn, not fail.
    Unpinned {
        /// `(filename, computed sha256)` for each unpinned file — paste into the catalog to
        /// pin it.
        actual: Vec<(String, String)>,
    },
}

/// The full catalog (same as [`KNOWN_MODELS`], as a function for symmetry with
/// [`installed_models`]).
pub fn known_models() -> &'static [ModelPack] {
    KNOWN_MODELS
}

/// The catalog entry for `id`, if any.
pub fn find_pack(id: &str) -> Option<&'static ModelPack> {
    KNOWN_MODELS.iter().find(|m| m.id == id)
}

/// The models-directory search path (see the module docs). Only directories that exist are
/// returned, in priority order; the list may be empty if none exist yet.
pub fn models_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(explicit) = std::env::var_os("CLEANROOM_LLM_MODELS_DIR") {
        out.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("models"));
        }
    }
    out.push(PathBuf::from("models"));
    out.retain(|d| d.is_dir());
    out
}

/// Every known pack whose primary file is present in one of the [`models_dirs`], in
/// [`KNOWN_MODELS`] order (so the 7B pack is preferred over the 1.5B when both are installed).
pub fn installed_models() -> Vec<InstalledModel> {
    let dirs = models_dirs();
    let mut out = Vec::new();
    for pack in KNOWN_MODELS {
        if let Some(path) = dirs
            .iter()
            .map(|d| d.join(pack.primary_filename()))
            .find(|p| p.is_file())
        {
            out.push(InstalledModel { pack: *pack, path });
        }
    }
    out
}

/// Locate an installed pack's primary `.gguf` by catalog `id`. `None` if the id is unknown or
/// the file is not present in any models dir. Never downloads.
pub fn locate_model(id: &str) -> Option<PathBuf> {
    let pack = find_pack(id)?;
    models_dirs()
        .iter()
        .map(|d| d.join(pack.primary_filename()))
        .find(|p| p.is_file())
}

/// Hash every file of a pack found in `dir` and compare against its pinned sha256.
///
/// This is what the models manager's **"verify files"** button calls after a download and on
/// demand. Multi-GB files are streamed in 1 MiB chunks, never slurped into memory. All shards
/// of a split pack must be present and correct; a missing shard is [`LlmError::ModelNotFound`].
///
/// Returns [`LlmError::HashMismatch`] when a pinned hash exists and does not match — a corrupt
/// or swapped pack must never be handed to llama.cpp.
pub fn verify_model_in(pack: &ModelPack, dir: &Path) -> Result<Verification, LlmError> {
    let mut unpinned = Vec::new();
    for file in pack.files {
        let path = dir.join(file.filename);
        let actual = sha256_file(&path)?;
        if file.sha256.is_empty() {
            tracing::warn!(
                pack = pack.id,
                file = file.filename,
                sha256 = %actual,
                "model file hash is not pinned in the catalog; integrity unverified"
            );
            unpinned.push((file.filename.to_string(), actual));
        } else if !actual.eq_ignore_ascii_case(file.sha256) {
            return Err(LlmError::HashMismatch {
                expected: file.sha256.to_string(),
                actual,
            });
        }
    }
    if unpinned.is_empty() {
        Ok(Verification::Verified)
    } else {
        Ok(Verification::Unpinned { actual: unpinned })
    }
}

/// Verify a **single-file** pack against `path` directly.
///
/// Convenience for the common case (and back-compat with the pre-split signature). Panics-free
/// but returns [`LlmError::ModelCorrupt`] if called on a multi-file pack, whose shards live at
/// fixed sibling names rather than one arbitrary path — use [`verify_model_in`] for those.
pub fn verify_model(pack: &ModelPack, path: &Path) -> Result<Verification, LlmError> {
    let [file] = pack.files else {
        return Err(LlmError::ModelCorrupt(format!(
            "{} is a {}-file pack; verify it with verify_model_in against its models dir",
            pack.id,
            pack.files.len()
        )));
    };
    let actual = sha256_file(path)?;
    if file.sha256.is_empty() {
        tracing::warn!(
            pack = pack.id,
            file = %path.display(),
            sha256 = %actual,
            "model pack hash is not pinned in the catalog; integrity unverified"
        );
        return Ok(Verification::Unpinned {
            actual: vec![(file.filename.to_string(), actual)],
        });
    }
    if !actual.eq_ignore_ascii_case(file.sha256) {
        return Err(LlmError::HashMismatch {
            expected: file.sha256.to_string(),
            actual,
        });
    }
    Ok(Verification::Verified)
}

/// Streaming sha256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, LlmError> {
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

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_apache_only_and_never_the_research_licensed_3b() {
        let mut ids: Vec<&str> = KNOWN_MODELS.iter().map(|m| m.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "pack ids must be unique");

        for pack in KNOWN_MODELS {
            assert!(
                !pack.files.is_empty(),
                "{} must list at least one file",
                pack.id
            );
            assert_eq!(pack.license, "Apache-2.0", "{} must be Apache-2.0", pack.id);
            // ADR-004: Qwen2.5-3B ships under a research license. It must never enter the
            // catalog, whatever a future contributor's benchmark says.
            assert!(
                !pack.id.contains("3b"),
                "Qwen2.5-3B is research-licensed and must not be shipped: {}",
                pack.id
            );
            for file in pack.files {
                assert!(file.filename.ends_with(".gguf"), "{}", file.filename);
                assert!(
                    !file.filename.contains("3b"),
                    "Qwen2.5-3B is research-licensed and must not be shipped: {}",
                    file.filename
                );
            }
            assert!(pack.context_tokens >= 8192, "{}", pack.id);
        }
        assert!(find_pack(DEFAULT_MODEL_ID).is_some());
        assert!(find_pack(LOW_RAM_MODEL_ID).is_some());
    }

    /// The release blocker this module closes: both shipping packs are now pinned — every file
    /// has an https URL *and* a well-formed sha256, and inference's `-m` target is a real gguf.
    #[test]
    fn shipping_packs_are_pinned_and_well_formed() {
        for pack in KNOWN_MODELS {
            assert!(
                pack.is_provisioned(),
                "{} must be fully pinned before release (empty url/sha256)",
                pack.id
            );
            assert!(
                pack.primary_filename().ends_with(".gguf"),
                "{} primary file must be a gguf",
                pack.id
            );
            assert!(pack.size_bytes() > 0, "{}", pack.id);
            for file in pack.files {
                assert!(
                    file.url.starts_with("https://"),
                    "{} url must be https: {}",
                    pack.id,
                    file.url
                );
                assert!(
                    file.url.ends_with(file.filename),
                    "{}: url should resolve to its filename ({} vs {})",
                    pack.id,
                    file.url,
                    file.filename
                );
                assert_eq!(
                    file.sha256.len(),
                    64,
                    "{} sha256 must be 32 bytes: {}",
                    pack.id,
                    file.filename
                );
                assert!(
                    file.sha256
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "{} sha256 must be lowercase hex: {}",
                    pack.id,
                    file.filename
                );
                assert!(file.size_bytes > 0, "{}: {}", pack.id, file.filename);
            }
        }
    }

    /// The default (7B) is the split upstream gguf; llama.cpp is pointed at shard 1.
    #[test]
    fn default_pack_is_the_split_7b_pointed_at_shard_one() {
        let pack = find_pack(DEFAULT_MODEL_ID).unwrap();
        assert_eq!(pack.files.len(), 2, "upstream 7B q4_k_m is a 2-shard gguf");
        assert!(pack.primary_filename().contains("00001-of-00002"));
        // sum of the shard sizes, sanity: ~4.7 GB
        assert!(pack.size_bytes() > 4_000 * MIB && pack.size_bytes() < 5_000 * MIB);

        let low = find_pack(LOW_RAM_MODEL_ID).unwrap();
        assert_eq!(low.files.len(), 1, "1.5B q4_k_m is a single file");
    }

    #[test]
    fn locate_model_rejects_unknown_id() {
        assert!(locate_model("does-not-exist").is_none());
        assert!(find_pack("qwen2.5-3b-instruct").is_none());
    }

    #[test]
    fn sha256_file_matches_the_known_digest_of_abc() {
        let path = std::env::temp_dir().join(format!("anvil-llm-hash-{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").expect("write fixture");
        let digest = sha256_file(&path).expect("hash");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // sha256("abc") and sha256("abcd") — the digests the verify tests pin against.
    const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA_ABCD: &str = "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589";

    #[test]
    fn verify_single_file_pack_reports_unpinned_and_catches_a_mismatch() {
        // `ModelPack.files` is `&'static`, so the fixtures must be `static` too.
        static UNPINNED: [ModelFile; 1] = [ModelFile {
            filename: "synthetic.gguf",
            url: "",
            sha256: "",
            size_bytes: 3,
        }];
        static WRONG: [ModelFile; 1] = [ModelFile {
            filename: "synthetic.gguf",
            url: "https://example/x",
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            size_bytes: 3,
        }];
        static GOOD: [ModelFile; 1] = [ModelFile {
            filename: "synthetic.gguf",
            url: "https://example/x",
            sha256: SHA_ABC,
            size_bytes: 3,
        }];

        let path = std::env::temp_dir().join(format!("anvil-llm-vfy-{}.gguf", std::process::id()));
        std::fs::write(&path, b"abc").expect("write fixture");

        let mut pack = KNOWN_MODELS[1]; // the single-file 1.5B, so the `[file]` guard passes

        // No pinned hash → Unpinned, carrying the computed digest.
        pack.files = &UNPINNED;
        let unpinned = verify_model(&pack, &path).expect("verify");
        assert!(matches!(unpinned, Verification::Unpinned { .. }));

        // Pinned but wrong → hard error, never a silent pass.
        pack.files = &WRONG;
        let err = verify_model(&pack, &path).expect_err("must reject a mismatched file");
        assert!(matches!(err, LlmError::HashMismatch { .. }));

        // Matching → Verified.
        pack.files = &GOOD;
        assert_eq!(
            verify_model(&pack, &path).expect("verify"),
            Verification::Verified
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `verify_model` refuses a multi-file pack — those must go through `verify_model_in`.
    #[test]
    fn verify_model_rejects_a_split_pack() {
        let path = std::env::temp_dir().join("nope.gguf");
        let split = find_pack(DEFAULT_MODEL_ID).unwrap(); // 2 shards
        assert!(matches!(
            verify_model(split, &path),
            Err(LlmError::ModelCorrupt(_))
        ));
    }

    /// A split pack is verified against its models dir; every shard must match.
    #[test]
    fn verify_model_in_checks_every_shard() {
        static SPLIT: [ModelFile; 2] = [
            ModelFile {
                filename: "part-00001-of-00002.gguf",
                url: "https://example/1",
                sha256: SHA_ABC,
                size_bytes: 3,
            },
            ModelFile {
                filename: "part-00002-of-00002.gguf",
                url: "https://example/2",
                sha256: SHA_ABCD,
                size_bytes: 4,
            },
        ];

        let dir = std::env::temp_dir().join(format!("anvil-llm-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("part-00001-of-00002.gguf"), b"abc").unwrap();
        std::fs::write(dir.join("part-00002-of-00002.gguf"), b"abcd").unwrap();

        let mut pack = KNOWN_MODELS[0];
        pack.files = &SPLIT;
        assert_eq!(
            verify_model_in(&pack, &dir).expect("verify"),
            Verification::Verified
        );

        // Corrupt shard 2 → mismatch.
        std::fs::write(dir.join("part-00002-of-00002.gguf"), b"XXXX").unwrap();
        assert!(matches!(
            verify_model_in(&pack, &dir),
            Err(LlmError::HashMismatch { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
