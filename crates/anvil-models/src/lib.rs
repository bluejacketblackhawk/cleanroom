//! # anvil-models — the sanctioned model-download module (ADR-009 airplane-mode carve-out)
//!
//! The **one** place Cleanroom fetches model weights over the network. Every audio/AI/media/ASR
//! engine crate stays HTTP-client-free by CI gate (`.github/scripts/check-network-deps.sh`);
//! this crate is that gate's named `models` carve-out. Both the desktop Models screen
//! (`apps/desktop/src-tauri/src/models.rs`) and the headless `anvil models pull`
//! (`crates/anvil-cli`) call it, so the two never tell divergent stories about *where* a
//! weight comes from, *how* it's verified, or *where on disk* it lands.
//!
//! ## Where downloads land (never inside a signed `.app`)
//! [`models_dir`] is the per-user, OS-appropriate, always-writable config directory
//! (`~/Library/Application Support/anvil/models` on macOS, `%APPDATA%\anvil\models` on
//! Windows) — the **same** directory [`anvil_asr::models_dirs`] searches first, so a pack this
//! module downloads is immediately visible to the transcribe path with no extra wiring.
//!
//! Critically it is **never** the macOS `.app` bundle's `Contents/Resources`: that path is
//! unwritable by a user who dragged the DMG to `/Applications` without admin rights, *and*
//! writing into it would break the code signature and fail Gatekeeper on the next launch.
//! A naive "download next to the executable" design — which is user-writable on a Windows
//! per-user install — therefore has no viable Mac equivalent, which is exactly why the
//! download destination is the config dir on every platform.
//!
//! ## What it downloads + how it's verified
//! The whisper.cpp ggml packs from [`anvil_asr::KNOWN_MODELS`] — that catalog is the single
//! source of truth for url + sha1 (whisper.cpp publishes sha1, not sha256). We stream to
//! `<filename>.part` with an HTTP `Range` resume when a partial exists, sha1-verify the
//! finished file against the catalog pin, and only then atomically rename it into place. A
//! checksum mismatch deletes the partial and fails closed — never an unverified install.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anvil_core::platform::Platform;
use sha1::{Digest, Sha1};

/// A progress tick during a fetch, for a UI bar or a CLI counter. Terminal states
/// (installed / paused) are the [`Outcome`], not a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Bytes are streaming to disk (`downloaded` of `total`).
    Downloading,
    /// The download is complete; the file is being hashed to verify it.
    Verifying,
}

/// How a [`fetch_whisper_model`] call ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The pack downloaded and hash-verified; here's where it landed and its proof.
    Installed(Installed),
    /// The caller's cancel flag was set mid-download. A resumable `<filename>.part` is left on
    /// disk so the next call continues rather than restarting (04 §S7 "download w/ resume").
    Paused,
}

/// Proof a pack finished downloading and passed verification — the desktop writes this into
/// its installed-sentinel, the CLI prints it.
#[derive(Debug, Clone)]
pub struct Installed {
    /// Absolute path to the installed `ggml-*.bin`.
    pub path: PathBuf,
    /// Lowercase-hex sha1 of the installed file — equal, by construction, to the catalog pin.
    pub sha1: String,
    /// Size of the installed file in bytes.
    pub size_bytes: u64,
}

/// Everything that can go wrong fetching a pack. A hard error leaves no verified install
/// behind (a checksum mismatch also deletes the corrupt partial).
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// `pack_id` doesn't name a known whisper ggml pack (see [`anvil_asr::KNOWN_MODELS`]).
    #[error("unknown whisper model pack: {0}")]
    UnknownPack(String),
    /// The models directory couldn't be created.
    #[error("could not create {0}: {1}")]
    CreateDir(PathBuf, #[source] std::io::Error),
    /// The blocking HTTP client failed to build.
    #[error("could not start the download client: {0}")]
    Client(String),
    /// The request never reached the server (DNS, TLS, connect, or a dropped socket).
    #[error("could not reach {0}: {1}")]
    Unreachable(String, String),
    /// The server answered, but not with success (and not the benign 416 "already complete").
    #[error("download failed: HTTP {0} from {1}")]
    HttpStatus(u16, String),
    /// A local filesystem error reading/writing the partial or the installed file.
    #[error("download error: {0}")]
    Io(#[from] std::io::Error),
    /// The finished file's sha1 didn't match the pinned catalog value — corrupted or tampered.
    #[error(
        "checksum mismatch for {name} (expected {expected}, got {got}) — the download was \
         corrupted or incomplete; try again"
    )]
    Checksum {
        name: String,
        expected: String,
        got: String,
    },
}

/// The per-user, always-writable models directory (never the signed `.app` bundle — see the
/// module docs). This is the same location [`anvil_asr::models_dirs`] searches first and the
/// desktop's `ModelsState` uses, so a downloaded pack is found by the transcribe path, the
/// `installed_models` enumeration, and `anvil models list` alike.
pub fn models_dir() -> PathBuf {
    anvil_core::platform::current().config_dir().join("models")
}

/// Resolve a Models-screen pack id (`"whisper-small"`) **or** a raw `anvil_asr` catalog id
/// (`"small"`, `"small.en"`) to its catalog entry. `None` for anything that is neither.
fn resolve_pack(pack_id: &str) -> Option<&'static anvil_asr::ModelPack> {
    let catalog_id = pack_id.strip_prefix("whisper-").unwrap_or(pack_id);
    anvil_asr::known_models()
        .iter()
        .find(|m| m.id == catalog_id)
}

/// Whether `pack_id` names a whisper ggml pack this module knows how to fetch (accepts both
/// the `"whisper-"`-prefixed Models-screen id and the bare `anvil_asr` catalog id).
pub fn is_whisper_pack(pack_id: &str) -> bool {
    resolve_pack(pack_id).is_some()
}

/// The catalog filename a pack installs as (e.g. `"ggml-small.bin"`), or `None` if `pack_id`
/// names no known whisper pack. Lets a caller check "is this already on disk?" without a fetch.
pub fn whisper_filename(pack_id: &str) -> Option<&'static str> {
    resolve_pack(pack_id).map(|m| m.filename)
}

/// Download + sha1-verify a whisper ggml pack into `dir` (typically [`models_dir`]).
///
/// `pack_id` is either a Models-screen id (`"whisper-small"`) or a bare catalog id
/// (`"small"`). Streams the weight to `<filename>.part`, resuming from a prior partial via an
/// HTTP `Range` request, verifies the finished file's sha1 against the [`anvil_asr::KNOWN_MODELS`]
/// pin, then atomically renames it to its catalog filename. `cancel` (checked each chunk)
/// leaves a resumable partial and returns [`Outcome::Paused`]; a hard failure returns a
/// [`DownloadError`] and installs nothing.
///
/// `on_tick` fires on every streamed chunk and once at the verify step, so a UI can show a
/// live bar and a CLI a counter — the terminal *installed* / *paused* states come back as the
/// [`Outcome`], not as a tick.
pub fn fetch_whisper_model(
    pack_id: &str,
    dir: &Path,
    cancel: &AtomicBool,
    mut on_tick: impl FnMut(u64, u64, Tick),
) -> Result<Outcome, DownloadError> {
    let pack =
        resolve_pack(pack_id).ok_or_else(|| DownloadError::UnknownPack(pack_id.to_string()))?;
    std::fs::create_dir_all(dir).map_err(|e| DownloadError::CreateDir(dir.to_path_buf(), e))?;
    let part = dir.join(format!("{}.part", pack.filename));
    let total = pack.size_bytes.max(1);

    tracing::info!(pack = pack.id, url = pack.url, "starting model download");

    let mut downloaded = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    on_tick(downloaded, total, Tick::Downloading);

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("anvil-models/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| DownloadError::Client(e.to_string()))?;

    let mut request = client.get(pack.url);
    if downloaded > 0 {
        request = request.header("Range", format!("bytes={downloaded}-"));
    }
    let response = request
        .send()
        .map_err(|e| DownloadError::Unreachable(pack.url.to_string(), e.to_string()))?;

    let status = response.status().as_u16();
    let resumed = status == 206;
    if downloaded > 0 && !resumed {
        // The server ignored our Range (200 OK with the full body) or it no longer applies
        // (416) — restart clean rather than risk appending a second copy onto the partial.
        downloaded = 0;
        let _ = std::fs::remove_file(&part);
    }
    if !response.status().is_success() && status != 416 {
        return Err(DownloadError::HttpStatus(status, pack.url.to_string()));
    }
    if status != 416 {
        // (416 == the existing `.part` is already the whole file; fall through to verify.)
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&part)?;
        let mut reader = response;
        let mut buf = [0u8; 64 * 1024];
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Ok(Outcome::Paused);
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            downloaded += n as u64;
            on_tick(downloaded, total, Tick::Downloading);
        }
    }

    on_tick(downloaded, total, Tick::Verifying);
    let (got, size_bytes) = sha1_file(&part)?;
    if !got.eq_ignore_ascii_case(pack.sha1) {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::Checksum {
            name: pack.display_name.to_string(),
            expected: pack.sha1.to_string(),
            got,
        });
    }

    let final_path = dir.join(pack.filename);
    std::fs::rename(&part, &final_path)?;
    Ok(Outcome::Installed(Installed {
        path: final_path,
        sha1: got,
        size_bytes,
    }))
}

/// Stream a file's sha1 in 1 MiB chunks (these run to hundreds of MB / a couple GB — never
/// slurp the whole thing into memory) and return `(lowercase_hex_sha1, size_bytes)`.
fn sha1_file(path: &Path) -> Result<(String, u64), DownloadError> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_is_the_per_user_app_config_dir() {
        let dir = models_dir();
        assert!(dir.ends_with("models"), "{dir:?}");
        assert!(
            dir.parent().unwrap().ends_with("cleanroom"),
            "download dir must be namespaced under the app config dir (cleanroom), got {dir:?}"
        );
    }

    #[test]
    fn resolve_pack_accepts_screen_and_catalog_ids() {
        assert_eq!(resolve_pack("whisper-small").map(|m| m.id), Some("small"));
        assert_eq!(resolve_pack("small").map(|m| m.id), Some("small"));
        assert_eq!(resolve_pack("small.en").map(|m| m.id), Some("small.en"));
        assert!(resolve_pack("whisper-nope").is_none());
        assert!(resolve_pack("rnnoise").is_none());
    }

    #[test]
    fn is_whisper_pack_and_filename_agree_with_the_catalog() {
        assert!(is_whisper_pack("whisper-tiny"));
        assert!(is_whisper_pack("medium"));
        assert!(!is_whisper_pack("qwen2.5-7b-instruct-q4_k_m"));
        assert_eq!(whisper_filename("whisper-small"), Some("ggml-small.bin"));
        assert_eq!(whisper_filename("tiny"), Some("ggml-tiny.bin"));
        assert_eq!(whisper_filename("nope"), None);
    }

    #[test]
    fn fetch_rejects_unknown_pack_without_touching_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let err = fetch_whisper_model("does-not-exist", tmp.path(), &cancel, |_, _, _| {})
            .expect_err("unknown pack must error before any request");
        assert!(matches!(err, DownloadError::UnknownPack(_)), "{err:?}");
    }

    #[test]
    fn sha1_file_matches_a_known_vector() {
        // sha1("abc") == a9993e364706816aba3e25717850c26c9cd0d89d (FIPS-180 example).
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        let (got, size) = sha1_file(&p).unwrap();
        assert_eq!(got, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(size, 3);
    }

    /// End-to-end real-network fetch of the smallest real pack (`whisper-tiny`, ~75 MB): only
    /// runs when explicitly opted in (`CLEANROOM_TEST_REAL_MODEL_DOWNLOAD=1`), since a network
    /// fetch has no place in the default `cargo test` loop. Proves the resume-from-cancel path
    /// and the sha1-verified install against the real Hugging Face URL + catalog pin.
    #[test]
    fn fetch_whisper_tiny_resumes_and_verifies_for_real() {
        if std::env::var_os("CLEANROOM_TEST_REAL_MODEL_DOWNLOAD").is_none() {
            eprintln!("skipping: CLEANROOM_TEST_REAL_MODEL_DOWNLOAD not set (real network fetch)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        // Cancel from *inside* the progress callback the moment real bytes land — a fixed sleep
        // races a fast CDN (the whole 75 MB can arrive in well under a second), which would
        // either finish before the cancel or trip it before any chunk is written. Cancelling on
        // the first non-zero tick guarantees a non-empty, resumable `.part`.
        let cancel = AtomicBool::new(false);
        let outcome1 =
            fetch_whisper_model("whisper-tiny", tmp.path(), &cancel, |downloaded, _, _| {
                if downloaded > 0 {
                    cancel.store(true, Ordering::SeqCst);
                }
            })
            .unwrap();
        let part = tmp.path().join("ggml-tiny.bin.part");
        match outcome1 {
            // The expected path: a mid-stream cancel left a resumable partial.
            Outcome::Paused => {
                assert!(part.is_file() && part.metadata().unwrap().len() > 0);
            }
            // Tolerated: the fetch genuinely completed in one streamed pass before the cancel
            // took effect on the next loop turn — still a valid, verified install.
            Outcome::Installed(_) => {}
        }

        // Resume (or, if already installed, no-op re-verify) to completion + verify.
        let done = AtomicBool::new(false);
        let outcome = fetch_whisper_model("whisper-tiny", tmp.path(), &done, |_, _, _| {}).unwrap();
        match outcome {
            Outcome::Installed(i) => {
                assert!(i.path.ends_with("ggml-tiny.bin") && i.path.is_file());
                assert_eq!(i.sha1, "bd577a113a864445d4c299885e0cb97d4ba92b5f");
            }
            Outcome::Paused => panic!("second call should complete"),
        }
        assert!(!part.exists(), "the .part is renamed away on success");
    }
}
