//! Models manager (04 §S7): lists model packs with honest installed/not-installed state
//! and a real download action — HTTPS fetch with progress + resume (HTTP `Range`) + cancel +
//! checksum verification — for both the whisper transcription packs (single-file, sha1 against
//! whisper.cpp's own published checksums) and the Qwen shownotes LLM packs (possibly split,
//! multi-file, sha256 against `anvil_llm`'s catalog, via [`fetch_and_verify_llm`]).
//!
//! RNNoise ships compiled into the binary (`anvil_ai`, via `nnnoiseless`, MIT) — no download,
//! always "Installed". Whisper packs (`anvil_asr`, ASR) and the Qwen shownotes packs
//! (`anvil_llm`) are real, sized, licensed, hash-verified, and downloadable here. The
//! diarization ONNX models (`anvil_asr`, speaker labels) are listed with honest
//! installed-state but are **provisioned by the app installer** — the segmentation model is a
//! `.tar.bz2` needing extraction and the sherpa sidecar binary ships alongside it, both the
//! provisioning lane's job — so their rows report state rather than downloading.
//!
//! ## Where a download lands (and who fetches it)
//! The actual HTTPS fetch + sha1 verify is **not** implemented here — it lives in the
//! sanctioned [`anvil_models`] network crate (the ADR-009 airplane-mode carve-out), shared
//! byte-for-byte with `anvil models pull` so the desktop and the CLI never tell divergent
//! stories. This module is the thin Tauri wrapper: it drives [`anvil_models::fetch_whisper_model`]
//! and turns its ticks into `models://download` events + an installed sentinel.
//!
//! A verified pack lands in [`anvil_models::models_dir`] (`~/Library/Application Support/anvil/
//! models` on macOS) under its `anvil_asr` catalog filename (e.g. `ggml-small.bin`). That is the
//! **same** directory `anvil_asr::models_dirs()` now searches first, so a downloaded pack is
//! immediately found by the transcribe path via `anvil_asr::locate_model` — no desktop-only
//! fallback needed, and never inside the signed `.app` bundle (which is unwritable and would
//! break the code signature).
//!
//! ## Checksum provenance
//! The url + pinned sha1 for every whisper pack are the single source of truth in
//! `anvil_asr::KNOWN_MODELS` (whisper.cpp publishes sha1, not sha256); [`anvil_models`] verifies
//! against them. The display rows in [`MANIFEST`] below carry no download coordinates of their
//! own — they map to the catalog by id (enforced by a test) so the two can't drift. A
//! verification failure fails closed rather than installing unverified bytes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Static catalog of every model pack the S7 screen — and the Transcript tab's model
/// picker, which filters this list to `kind == "asr"` — can offer. Sizes/licenses per
/// `handoff/02-ARCHITECTURE.md` §Model runtime.
struct ManifestEntry {
    id: &'static str,
    name: &'static str,
    detail: &'static str,
    size_label: &'static str,
    size_bytes: u64,
    license: &'static str,
    kind: &'static str,
    downloadable: bool,
    /// Milestone that lands install support, for rows that aren't downloadable yet.
    arrives: Option<&'static str>,
}

const MANIFEST: &[ManifestEntry] = &[
    ManifestEntry {
        id: "rnnoise",
        name: "RNNoise (Fast denoise)",
        detail: "Speech denoise model behind the Fast tier. Compiled into the app — nothing to download, works with no network ever.",
        size_label: "< 1 MB (built in)",
        size_bytes: 0,
        license: "MIT (nnnoiseless)",
        kind: "denoise",
        downloadable: false,
        arrives: None,
    },
    ManifestEntry {
        id: "whisper-tiny",
        name: "Whisper — Tiny",
        detail: "Fastest transcription pass. Good for a quick draft or a low-power machine.",
        size_label: "75 MB",
        size_bytes: 75_000_000,
        license: "MIT (whisper.cpp ggml weights)",
        kind: "asr",
        downloadable: true,
        arrives: None,
    },
    ManifestEntry {
        id: "whisper-base",
        name: "Whisper — Base",
        detail: "A step up in accuracy over Tiny, still fast on CPU.",
        size_label: "142 MB",
        size_bytes: 142_000_000,
        license: "MIT (whisper.cpp ggml weights)",
        kind: "asr",
        downloadable: true,
        arrives: None,
    },
    ManifestEntry {
        id: "whisper-small",
        name: "Whisper — Small",
        detail: "The balanced default: noticeably better on accents and noisy rooms.",
        size_label: "466 MB",
        size_bytes: 466_000_000,
        license: "MIT (whisper.cpp ggml weights)",
        kind: "asr",
        downloadable: true,
        arrives: None,
    },
    ManifestEntry {
        id: "whisper-medium",
        name: "Whisper — Medium",
        detail: "Best accuracy. Worth it for archival transcripts; slower on CPU.",
        size_label: "1.5 GB",
        size_bytes: 1_500_000_000,
        license: "MIT (whisper.cpp ggml weights)",
        kind: "asr",
        downloadable: true,
        arrives: None,
    },
    // Shownotes LLM (Qwen2.5) — the file list, URLs and sha256s live in `anvil_llm`'s catalog
    // (single source of truth); these rows just carry display copy and reference it by id.
    // Real multi-file sha256 download + verify (`run_download_llm`); use path is
    // `shownotes::generate_shownotes`.
    ManifestEntry {
        id: anvil_llm::DEFAULT_MODEL_ID, // "qwen2.5-7b-instruct-q4_k_m"
        name: "Shownotes — Qwen2.5 7B",
        detail: "The good local model for episode summaries, chapter markers, and title suggestions. Fully optional — Cleanroom masters and exports (and writes basic shownotes) without it.",
        size_label: "4.7 GB",
        size_bytes: 4_683_073_632,
        license: "Apache-2.0 (Qwen2.5-7B-Instruct)",
        kind: "llm",
        downloadable: true,
        arrives: None,
    },
    ManifestEntry {
        id: anvil_llm::LOW_RAM_MODEL_ID, // "qwen2.5-1.5b-instruct-q4_k_m"
        name: "Shownotes — Qwen2.5 1.5B (low RAM)",
        detail: "The low-RAM shownotes model for 8 GB machines. Weaker than the 7B, still a real AI summary. Optional.",
        size_label: "1.1 GB",
        size_bytes: 1_117_320_736,
        license: "Apache-2.0 (Qwen2.5-1.5B-Instruct)",
        kind: "llm",
        downloadable: true,
        arrives: None,
    },
    // Speaker diarization ONNX models (04 §S2 "speaker labels"). The segmentation model ships
    // as a `.tar.bz2` that needs extraction, and the diarization sidecar binary is provisioned
    // with the app install — both the other lane's job — so these rows report honest
    // installed/not-installed state but are provisioned by the installer, not downloaded here.
    ManifestEntry {
        id: "pyannote-segmentation-3.0",
        name: "Speaker segmentation (pyannote 3.0)",
        detail: "Finds who is speaking when, for the Transcript tab's speaker labels. Comes with the app install.",
        size_label: "6 MB",
        size_bytes: 5_992_913,
        license: "MIT (pyannote/segmentation-3.0, © 2022 CNRS)",
        kind: "diarize",
        downloadable: false,
        arrives: None,
    },
    ManifestEntry {
        id: "titanet-small",
        name: "Speaker embeddings (NeMo TitaNet-small)",
        detail: "Tells the speakers apart once they're found. Comes with the app install.",
        size_label: "40 MB",
        size_bytes: 40_257_283,
        license: "CC-BY-4.0 (NVIDIA NeMo — attribution required)",
        kind: "diarize",
        downloadable: false,
        arrives: None,
    },
];

fn find_entry(id: &str) -> Option<&'static ManifestEntry> {
    MANIFEST.iter().find(|m| m.id == id)
}

/// Map a Models-screen ASR pack id (`ManifestEntry::id`, e.g. `"whisper-small"`) to the
/// `anvil_asr` catalog filename it installs as (e.g. `"ggml-small.bin"` — the multilingual
/// variant, matching [`ManifestEntry::url`]). Also accepts a raw `anvil_asr` catalog id with
/// no `"whisper-"` prefix (e.g. `"small"`) directly. `None` for anything that maps to
/// neither. Shared with `transcript::transcribe`'s model resolution — see the module docs.
pub(crate) fn asr_ggml_filename(pack_id: &str) -> Option<&'static str> {
    let asr_id = pack_id.strip_prefix("whisper-").unwrap_or(pack_id);
    anvil_asr::known_models()
        .iter()
        .find(|m| m.id == asr_id)
        .map(|m| m.filename)
}

/// One row in the S7 model card grid (and, filtered by `kind`, the Transcript tab's model
/// picker).
#[derive(Debug, Clone, Serialize)]
pub struct ModelPack {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub size: &'static str,
    pub size_bytes: u64,
    pub license: &'static str,
    /// "denoise" | "asr" | "llm" — lets the Transcript tab's model picker show only ASR
    /// packs without hardcoding id prefixes.
    pub kind: &'static str,
    pub installed: bool,
    pub downloadable: bool,
    /// The milestone that lands install support, shown on a disabled row. `None` once
    /// install is wired (every `downloadable` pack, plus the always-installed RNNoise).
    pub arrives: Option<&'static str>,
    /// Bytes already downloaded toward this pack from a prior in-progress or cancelled
    /// download; 0 once installed or if nothing has started.
    pub downloaded_bytes: u64,
    /// This pack is provisioned by the app installer, not downloaded from the Models screen
    /// (the diarization models — a `.tar.bz2` needing extraction plus the sidecar binary —
    /// are the provisioning lane's job). The UI shows a "comes with the app" note, not a
    /// download button, and installed state is still reported honestly.
    pub installer_provisioned: bool,
}

/// Where model packs live on disk, plus the cancel flags for any in-flight downloads.
/// Created lazily by `run_download`, not at startup (04 §S7 pattern mirrors
/// `PresetsState`: a user who never downloads a model never gets an empty folder).
pub struct ModelsState {
    dir: PathBuf,
    downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl ModelsState {
    pub fn new() -> Self {
        // The one canonical, user-writable, mac-safe models dir — the same path
        // `anvil_asr::models_dirs()` searches first and `anvil models pull` writes to, so a
        // pack downloaded here is found by the transcribe path with no desktop-only wiring.
        Self {
            dir: anvil_models::models_dir(),
            downloads: Mutex::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Default for ModelsState {
    fn default() -> Self {
        Self::new()
    }
}

/// On-disk proof a pack finished downloading and passed verification.
#[derive(Debug, Serialize, Deserialize)]
struct InstalledSentinel {
    /// Hex sha1 of the fully-downloaded file — verified against [`ManifestEntry::sha1`]
    /// before this sentinel is written (see the module docs on checksum provenance).
    sha1: String,
    size_bytes: u64,
}

fn sentinel_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.installed.json"))
}

fn progress_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.progress.json"))
}

fn is_installed(dir: &Path, id: &str) -> bool {
    sentinel_path(dir, id).is_file()
}

/// Whether a whisper ASR pack counts as installed: a verified download sentinel in `dir`,
/// **or** the real ggml file present on `anvil_asr`'s search path (a bundled/dev/env model, or
/// a bare hand-dropped `.bin` with no sentinel). Mirrors how [`llm_installed`] / [`diar_installed`]
/// honor their engine crates' locators, so any model transcription can actually *use* reads as
/// installed — not only one this screen downloaded. `anvil_asr::locate_model` searches
/// `CLEANROOM_WHISPER_MODELS_DIR`, the per-user config dir where downloads land, the `.app` bundle's
/// `Resources/models`, and the cwd (see `anvil_asr::models_dirs`).
fn whisper_installed(dir: &Path, id: &str) -> bool {
    if is_installed(dir, id) {
        return true;
    }
    let catalog_id = id.strip_prefix("whisper-").unwrap_or(id);
    anvil_asr::locate_model(catalog_id).is_some()
}

/// Whether a Qwen shownotes pack is installed: present in `anvil_llm`'s own search path (env /
/// exe-relative / cwd `models/`) or fully downloaded into the Models-screen dir. Checks real
/// files, so it's honest whether the pack arrived via the download here or was dropped in
/// manually — no sentinel to trust.
fn llm_installed(dir: &Path, id: &str) -> bool {
    if anvil_llm::locate_model(id).is_some() {
        return true;
    }
    match anvil_llm::model::find_pack(id) {
        Some(pack) => {
            !pack.files.is_empty() && pack.files.iter().all(|f| dir.join(f.filename).is_file())
        }
        None => false,
    }
}

/// Whether a diarization ONNX model is installed: present in `anvil_asr`'s diarization search
/// path or dropped into the Models-screen dir. These are installer-provisioned, so this only
/// ever *reports* state — the Models screen doesn't download them.
fn diar_installed(dir: &Path, id: &str) -> bool {
    if anvil_asr::locate_diarization_model(id).is_some() {
        return true;
    }
    match anvil_asr::known_diarization_models()
        .iter()
        .find(|p| p.id == id)
    {
        Some(pack) => dir.join(pack.filename).is_file(),
        None => false,
    }
}

/// Real installed state for a manifest row, dispatched on its `kind`: RNNoise is compiled in,
/// whisper packs prove install with a verified sentinel, and the LLM/diarization packs check
/// their engine crates' catalogs against the real files on disk.
fn pack_installed(dir: &Path, entry: &ManifestEntry) -> bool {
    match entry.kind {
        "denoise" => entry.arrives.is_none(), // compiled in (RNNoise)
        "asr" => whisper_installed(dir, entry.id),
        "llm" => llm_installed(dir, entry.id),
        "diarize" => diar_installed(dir, entry.id),
        _ => false,
    }
}

/// Bytes already saved from a previous partial download, or 0 if none (04 §S7 "resume").
fn read_progress(dir: &Path, id: &str) -> u64 {
    std::fs::read_to_string(progress_path(dir, id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_progress(dir: &Path, id: &str, bytes: u64) {
    let _ = std::fs::write(progress_path(dir, id), bytes.to_string());
}

/// Every pack the UI can offer, with real installed/in-progress state read off `dir`. A
/// free function (rather than inlined in the command below) so it's testable without a
/// running Tauri app — see `presets.rs`'s `resolve_preset_ref` for the same shape.
fn build_model_list(dir: &Path) -> Vec<ModelPack> {
    MANIFEST
        .iter()
        .map(|m| {
            // Installed state is per-kind: compiled-in (RNNoise), a verified sentinel (whisper),
            // or the engine catalog against real files (LLM/diarization). See `pack_installed`.
            let installed = pack_installed(dir, m);
            let downloaded_bytes = if installed || !m.downloadable {
                0
            } else {
                read_progress(dir, m.id)
            };
            ModelPack {
                id: m.id,
                name: m.name,
                detail: m.detail,
                size: m.size_label,
                size_bytes: m.size_bytes,
                license: m.license,
                kind: m.kind,
                installed,
                downloadable: m.downloadable,
                arrives: m.arrives,
                downloaded_bytes,
                installer_provisioned: m.kind == "diarize",
            }
        })
        .collect()
}

#[tauri::command]
pub fn models_list(state: State<'_, ModelsState>) -> Vec<ModelPack> {
    build_model_list(state.dir())
}

/// Progress ticks emitted on `models://download` while [`download_model`] runs, so the S7
/// screen (and the Transcript tab's inline "will download X MB" prompt) can show a live
/// bar without polling.
#[derive(Debug, Clone, Serialize)]
struct DownloadProgressEvent {
    pack: String,
    downloaded_bytes: u64,
    total_bytes: u64,
    /// "downloading" | "verifying" | "done" | "paused" | "error".
    status: &'static str,
    message: Option<String>,
}

fn emit_progress(
    app: &AppHandle,
    pack: &str,
    downloaded_bytes: u64,
    total_bytes: u64,
    status: &'static str,
    message: Option<String>,
) {
    let _ = app.emit(
        "models://download",
        DownloadProgressEvent {
            pack: pack.to_string(),
            downloaded_bytes,
            total_bytes,
            status,
            message,
        },
    );
}

/// Start (or resume) downloading `pack` in the background, returning immediately —
/// progress streams over `models://download` the same way batch/watch progress does
/// (`lib.rs::spawn_progress_poller`), just event-per-tick instead of polled.
#[tauri::command]
pub fn download_model(
    pack: String,
    app: AppHandle,
    state: State<'_, ModelsState>,
) -> Result<(), String> {
    let entry = find_entry(&pack).ok_or_else(|| format!("unknown model pack: {pack}"))?;
    if !entry.downloadable {
        if entry.kind == "diarize" {
            return Err(format!(
                "{} comes with the app install (it's provisioned with the speaker-ID sidecar), \
                 not downloaded here.",
                entry.name
            ));
        }
        return Err(format!(
            "{} isn't available to download yet{}",
            entry.name,
            entry
                .arrives
                .map(|m| format!(" — lands in {m}"))
                .unwrap_or_default()
        ));
    }
    if pack_installed(state.dir(), entry) {
        return Ok(()); // already installed — nothing to do
    }

    let cancel = Arc::new(AtomicBool::new(false));
    state
        .downloads
        .lock()
        .map_err(|_| "downloads lock poisoned")?
        .insert(entry.id.to_string(), Arc::clone(&cancel));

    let dir = state.dir().to_path_buf();
    // Whisper packs are single-file + sha1; the Qwen LLM packs are (possibly split) multi-file
    // + sha256 (verified via `anvil_llm::verify_model_in`). Both stream to disk with resume.
    match entry.kind {
        "llm" => std::thread::spawn(move || run_download_llm(&dir, entry, cancel, &app)),
        _ => std::thread::spawn(move || run_download(&dir, entry, cancel, &app)),
    };
    Ok(())
}

/// Cancel an in-flight download, leaving its progress on disk so a later `download_model`
/// call resumes rather than restarting (04 §S7 "download w/ progress+resume").
#[tauri::command]
pub fn download_model_cancel(pack: String, state: State<'_, ModelsState>) -> bool {
    let guard = state.downloads.lock().ok();
    let Some(flag) = guard.and_then(|g| g.get(&pack).cloned()) else {
        return false;
    };
    flag.store(true, Ordering::SeqCst);
    true
}

/// Thin wrapper handing [`fetch_and_verify`] a callback that emits `models://download`, and
/// turning a hard error into an `"error"` progress event (the command itself already
/// returned `Ok(())` to the UI once the background thread started — this is the only place
/// left to report a download-time failure).
fn run_download(
    dir: &Path,
    entry: &'static ManifestEntry,
    cancel: Arc<AtomicBool>,
    app: &AppHandle,
) {
    if let Err(message) = fetch_and_verify(dir, entry, &cancel, |downloaded, total, status, msg| {
        emit_progress(app, entry.id, downloaded, total, status, msg);
    }) {
        emit_progress(
            app,
            entry.id,
            read_progress(dir, entry.id),
            entry.size_bytes.max(1),
            "error",
            Some(message),
        );
    }
}

/// Drive the shared [`anvil_models::fetch_whisper_model`] downloader (the real HTTPS fetch,
/// `Range` resume, sha1 verify against the `anvil_asr` catalog pin, and atomic install all live
/// there — byte-for-byte the same path `anvil models pull` runs), translating its ticks into
/// the desktop's `models://download` status strings and owning only the Tauri-facing artifacts:
/// the resume-progress counter [`build_model_list`] reads and the [`InstalledSentinel`]. A
/// cancel leaves a resumable `.part` and reports `"paused"`; a hard failure returns `Err` for
/// [`run_download`] to surface as an `"error"` event. Kept as a plain callback (not an
/// `AppHandle`) so it stays testable without a running Tauri app.
fn fetch_and_verify(
    dir: &Path,
    entry: &ManifestEntry,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64, &'static str, Option<String>),
) -> Result<(), String> {
    // The UI bar reads against the manifest's advertised size (e.g. "466 MB"); the shared
    // downloader ticks the real byte count, which we forward under that total.
    let total = entry.size_bytes.max(1);
    let outcome = anvil_models::fetch_whisper_model(
        entry.id,
        dir,
        cancel,
        |downloaded, _catalog_total, tick| {
            let status = match tick {
                anvil_models::Tick::Downloading => {
                    // Persist the resume counter so `build_model_list` can show "Resume
                    // download · N of M" after a pause or an app restart.
                    write_progress(dir, entry.id, downloaded);
                    "downloading"
                }
                anvil_models::Tick::Verifying => "verifying",
            };
            on_progress(downloaded, total, status, None);
        },
    )
    .map_err(|e| e.to_string())?;

    match outcome {
        anvil_models::Outcome::Paused => {
            on_progress(read_progress(dir, entry.id), total, "paused", None);
        }
        anvil_models::Outcome::Installed(installed) => {
            // The sentinel is this screen's proof-of-verified-install (the sha1 the shared
            // downloader already checked against the catalog pin); write it before the "done"
            // event so the UI's refresh-on-done sees `installed == true`.
            let sentinel = InstalledSentinel {
                sha1: installed.sha1,
                size_bytes: installed.size_bytes,
            };
            let json = serde_json::to_vec_pretty(&sentinel).map_err(|e| e.to_string())?;
            std::fs::write(sentinel_path(dir, entry.id), json)
                .map_err(|e| format!("could not write the installed marker: {e}"))?;
            let _ = std::fs::remove_file(progress_path(dir, entry.id));
            on_progress(installed.size_bytes, total, "done", None);
        }
    }
    Ok(())
}

/// Thin wrapper handing [`fetch_and_verify_llm`] a `models://download` emitter, mirroring
/// [`run_download`] for the multi-file Qwen shownotes packs.
fn run_download_llm(
    dir: &Path,
    entry: &'static ManifestEntry,
    cancel: Arc<AtomicBool>,
    app: &AppHandle,
) {
    tracing::info!(pack = entry.id, "starting shownotes model download");
    if let Err(message) =
        fetch_and_verify_llm(dir, entry, &cancel, |downloaded, total, status, msg| {
            emit_progress(app, entry.id, downloaded, total, status, msg);
        })
    {
        emit_progress(
            app,
            entry.id,
            read_progress(dir, entry.id),
            entry.size_bytes.max(1),
            "error",
            Some(message),
        );
    }
}

/// Download every file of a Qwen pack (its file list, URLs and pinned sha256s all come from
/// `anvil_llm`'s catalog — single source of truth, never duplicated here) into `dir` with
/// per-file HTTP `Range` resume, streaming each to `<filename>.part` and renaming on
/// completion, then verify the whole pack via [`anvil_llm::verify_model_in`] before it counts
/// as installed. sha256 (not the whisper path's sha1) because that is what upstream publishes.
///
/// Same honesty posture as [`fetch_and_verify`]: a cancel leaves resumable bytes, a failed
/// verification deletes the downloaded files so a retry re-fetches clean, and nothing is ever
/// marked installed unverified.
fn fetch_and_verify_llm(
    dir: &Path,
    entry: &ManifestEntry,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64, &'static str, Option<String>),
) -> Result<(), String> {
    let pack = anvil_llm::model::find_pack(entry.id)
        .ok_or_else(|| format!("no shownotes model catalog entry for {}", entry.id))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let total = pack.size_bytes().max(1);

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("anvil-desktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("could not start the download client: {e}"))?;

    // Bytes already finalized on disk (shards a previous run completed) — the resume baseline.
    let mut completed: u64 = pack
        .files
        .iter()
        .map(|f| {
            std::fs::metadata(dir.join(f.filename))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum();
    write_progress(dir, entry.id, completed);
    on_progress(completed, total, "downloading", None);

    for file in pack.files {
        let final_path = dir.join(file.filename);
        if final_path.is_file() {
            continue; // a shard a previous run already finished
        }
        let part = dir.join(format!("{}.part", file.filename));
        let mut downloaded = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

        let mut request = client.get(file.url);
        if downloaded > 0 {
            request = request.header("Range", format!("bytes={downloaded}-"));
        }
        let response = request
            .send()
            .map_err(|e| format!("could not reach {}: {e}", file.url))?;

        let status = response.status().as_u16();
        let resumed = status == 206;
        if downloaded > 0 && !resumed {
            // Server ignored the Range (200) or it no longer applies (416) — restart clean.
            downloaded = 0;
            let _ = std::fs::remove_file(&part);
        }
        if !response.status().is_success() && status != 416 {
            return Err(format!(
                "download failed: HTTP {} from {}",
                response.status(),
                file.url
            ));
        }
        if status != 416 {
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(resumed)
                .truncate(!resumed)
                .open(&part)
                .map_err(|e| format!("could not open {}: {e}", part.display()))?;
            let mut reader = response;
            let mut buf = [0u8; 64 * 1024];
            loop {
                if cancel.load(Ordering::SeqCst) {
                    write_progress(dir, entry.id, completed + downloaded);
                    on_progress(completed + downloaded, total, "paused", None);
                    return Ok(());
                }
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| format!("download error: {e}"))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .map_err(|e| format!("could not write {}: {e}", part.display()))?;
                downloaded += n as u64;
                write_progress(dir, entry.id, completed + downloaded);
                on_progress(completed + downloaded, total, "downloading", None);
            }
        }
        std::fs::rename(&part, &final_path)
            .map_err(|e| format!("could not install to {}: {e}", final_path.display()))?;
        completed += downloaded;
    }

    on_progress(completed, total, "verifying", None);
    if let Err(e) = anvil_llm::verify_model_in(pack, dir) {
        for file in pack.files {
            let _ = std::fs::remove_file(dir.join(file.filename));
        }
        let _ = std::fs::remove_file(progress_path(dir, entry.id));
        return Err(format!(
            "checksum verification failed for {} ({e}) — the download was corrupted; try again",
            entry.name
        ));
    }
    let _ = std::fs::remove_file(progress_path(dir, entry.id));
    on_progress(pack.size_bytes(), total, "done", None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_ids_are_unique() {
        let mut ids: Vec<&str> = MANIFEST.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MANIFEST.len());
    }

    #[test]
    fn every_downloadable_whisper_pack_maps_to_a_pinned_anvil_asr_catalog_entry() {
        // The manifest carries no url/sha1 of its own — the download coordinates are the single
        // source of truth in `anvil_asr::KNOWN_MODELS` (whisper.cpp publishes sha1), which
        // `anvil_models` verifies against. Enforce that every downloadable ASR row resolves to a
        // catalog pack, so a row can't silently name a pack the catalog doesn't pin.
        for m in MANIFEST
            .iter()
            .filter(|m| m.downloadable && m.kind == "asr")
        {
            assert!(
                anvil_models::is_whisper_pack(m.id),
                "{} has no anvil_asr catalog entry to fetch/verify against",
                m.id
            );
        }
    }

    #[test]
    fn every_downloadable_llm_pack_resolves_to_a_pinned_anvil_llm_catalog_entry() {
        for m in MANIFEST
            .iter()
            .filter(|m| m.downloadable && m.kind == "llm")
        {
            let pack = anvil_llm::model::find_pack(m.id)
                .unwrap_or_else(|| panic!("{} has no anvil_llm catalog entry", m.id));
            assert!(
                pack.is_provisioned(),
                "{} must be fully pinned (url + sha256) in anvil_llm",
                m.id
            );
        }
    }

    #[test]
    fn find_entry_finds_known_and_rejects_unknown() {
        assert!(find_entry("whisper-small").is_some());
        assert!(find_entry("does-not-exist").is_none());
    }

    #[test]
    fn asr_ggml_filename_maps_whisper_screen_ids_to_the_asr_catalog() {
        assert_eq!(asr_ggml_filename("whisper-tiny"), Some("ggml-tiny.bin"));
        assert_eq!(asr_ggml_filename("whisper-small"), Some("ggml-small.bin"));
        assert_eq!(asr_ggml_filename("whisper-medium"), Some("ggml-medium.bin"));
        assert_eq!(asr_ggml_filename("rnnoise"), None);
        assert_eq!(asr_ggml_filename("does-not-exist"), None);
    }

    #[test]
    fn progress_roundtrips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_progress(tmp.path(), "whisper-tiny"), 0);
        write_progress(tmp.path(), "whisper-tiny", 12_345);
        assert_eq!(read_progress(tmp.path(), "whisper-tiny"), 12_345);
    }

    #[test]
    fn models_list_reports_each_kind_with_the_right_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let list = build_model_list(tmp.path());

        // RNNoise: compiled in, always installed, nothing to download.
        let rnnoise = list.iter().find(|m| m.id == "rnnoise").unwrap();
        assert!(rnnoise.installed && !rnnoise.downloadable && !rnnoise.installer_provisioned);

        // A whisper pack: a downloadable ASR row. Install-state is detected globally now (a
        // bundled/dev/downloaded ggml counts — see `whisper_installed`), so this asserts the
        // static catalog shape rather than a machine-specific install-state.
        let tiny = list.iter().find(|m| m.id == "whisper-tiny").unwrap();
        assert!(tiny.downloadable && tiny.kind == "asr" && !tiny.installer_provisioned);

        // A Qwen shownotes pack: a downloadable LLM row, not installer-provisioned.
        let llm = list
            .iter()
            .find(|m| m.id == anvil_llm::LOW_RAM_MODEL_ID)
            .unwrap();
        assert!(llm.downloadable && llm.kind == "llm" && !llm.installer_provisioned);

        // A diarization pack: reported honestly, installer-provisioned, not a Models-screen
        // download.
        let diar = list
            .iter()
            .find(|m| m.id == "pyannote-segmentation-3.0")
            .unwrap();
        assert!(!diar.downloadable && diar.kind == "diarize" && diar.installer_provisioned);
    }

    #[test]
    fn llm_installed_detects_all_shards_present_in_the_download_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = anvil_llm::model::find_pack(anvil_llm::DEFAULT_MODEL_ID).unwrap();
        // No shards yet → not installed.
        assert!(!llm_installed(tmp.path(), pack.id));
        // Drop every shard as a fixture → installed (real files, no sentinel needed).
        for file in pack.files {
            std::fs::write(tmp.path().join(file.filename), b"fixture").unwrap();
        }
        assert!(llm_installed(tmp.path(), pack.id));
    }

    #[test]
    fn installed_sentinel_round_trips_sha1_field() {
        let sentinel = InstalledSentinel {
            sha1: "abc123".into(),
            size_bytes: 42,
        };
        let json = serde_json::to_string(&sentinel).unwrap();
        assert!(json.contains("\"sha1\""));
        let back: InstalledSentinel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sha1, "abc123");
        assert_eq!(back.size_bytes, 42);
    }

    /// End-to-end real-network test of the desktop wrapper (opt-in via
    /// `CLEANROOM_TEST_REAL_MODEL_DOWNLOAD=1` — a ~75 MB HTTPS fetch has no place in the default
    /// `cargo test` loop). The download/resume/verify itself is `anvil_models`' own tested
    /// concern; this proves the wrapper drives it, writes the installed sentinel, installs under
    /// the catalog filename, and clears the progress counter on success.
    #[test]
    fn fetch_and_verify_installs_and_writes_a_sentinel_for_a_real_pack() {
        if std::env::var_os("CLEANROOM_TEST_REAL_MODEL_DOWNLOAD").is_none() {
            eprintln!("skipping: CLEANROOM_TEST_REAL_MODEL_DOWNLOAD not set (real network fetch)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let entry = find_entry("whisper-tiny").unwrap();

        let cancel = AtomicBool::new(false);
        let mut statuses = Vec::new();
        fetch_and_verify(tmp.path(), entry, &cancel, |_, _, status, _| {
            statuses.push(status);
        })
        .unwrap();

        assert_eq!(statuses.last(), Some(&"done"));
        // The sentinel is the wrapper's own artifact (sentinel-scoped `is_installed`, not the
        // global `whisper_installed`, so this asserts *this* dir got one).
        assert!(is_installed(tmp.path(), entry.id));
        assert!(tmp.path().join("ggml-tiny.bin").is_file());
        assert!(
            !progress_path(tmp.path(), entry.id).exists(),
            "the resume counter is cleared on a successful install"
        );
    }
}
