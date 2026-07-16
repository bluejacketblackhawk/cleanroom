//! ANVIL Tauri shell. Commands are thin: they validate input and hand heavy work to the
//! Rust job system (worker threads + cancellation + `job://progress` events), returning
//! immediately. Audio playback never crosses the webview — the Rust side owns it via
//! `cpal` and the UI is a remote control (02 §Non-obvious consequences). M0 lane-C shipped
//! the playback surface (open/peaks/transport); M1 lane 3 added `master`/A-B; M2 adds real
//! encoders to Export plus the Batch (S4) / Watch (S5) / Presets (S6) / Models (S7, basic)
//! screens, in their own modules (`batch`, `watch`, `presets`, `models`, `export`) kept
//! thin over the `anvil_batch`/`anvil_media`/`anvil_project` engine crates. Any place a
//! backend piece still isn't real is marked `// INTEGRATION SEAM`. M3 adds the Transcript
//! tab's `transcribe`/`plan_cuts`/`apply_cuts`/`export_transcript` commands (`transcript`
//! module, stubbed against `anvil_asr`/`anvil_cut`) and a real download/resume/verify loop
//! for the Models screen's whisper packs (`models` module, stubbed against a real fetch).
//! M4 adds the last three desktop screens: S3 Multitrack (`multitrack` module — real
//! decode/peaks/solo/mute/gain/ducking and mixdown, with real GCC-PHAT alignment + clock-drift
//! repair from the full `anvil_multitrack` crate), Clip Studio (`clip_studio` module — a real
//! ffmpeg-composed MP4 render), and S10 Recording Guard (`guard` module — a real `cpal`
//! input capture added directly in this crate, since `anvil-audio` only does output).
//! The Chapters & Metadata tab (`metadata` module — `anvil_media`'s `TagEditor` + ffmpeg
//! chapters), speaker diarization (`transcript::diarize` over `anvil_asr::diarize` +
//! `assign_speakers`), and AI shownotes (`shownotes` module over `anvil_llm`, degrading to its
//! extractive fallback when no Qwen model is installed) are wired here over those engine crates.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use anvil_audio::{PeaksPyramid, PlaybackEngine};
use anvil_core::platform::Platform;
use anvil_dsp::MasterReport;
use anvil_media::AudioBuffer;
use anvil_project::Preset;
use serde::Serialize;
use tauri::{Emitter, Manager, State};

mod batch;
mod clip_studio;
mod diagnostics;
mod export;
mod guard;
mod metadata;
mod models;
mod multitrack;
mod presets;
mod settings;
mod shownotes;
mod transcript;
mod watch;

/// Filename of the ONNX Runtime dynamic library ANVIL bundles. On Intel (x86_64) macOS,
/// `anvil-ai`'s `ort` is built `load-dynamic` (see `crates/anvil-ai/Cargo.toml`): `ort` ships no
/// prebuilt Intel-mac binary, so instead of linking onnxruntime it resolves the library at RUN
/// time from `ORT_DYLIB_PATH`. ANVIL already bundles exactly one onnxruntime — Microsoft's
/// universal2 1.17.1, next to the sherpa diarization sidecar at `../Resources/sherpa/lib/` — and
/// that single copy also serves the in-process `ort` session, so the bundle never carries two.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "x86_64")),
    allow(dead_code)
)]
const ORT_DYLIB_NAME: &str = "libonnxruntime.1.17.1.dylib";

/// Resolve the bundled onnxruntime dylib relative to the executable's directory, returning `Some`
/// only when it actually exists. In a packaged `.app` the exe is at `Contents/MacOS/<exe>` and the
/// sherpa lane stages its dylib at `../Resources/sherpa/lib/<name>` — the same copy
/// `DiarizeSidecar` loads via the sherpa binary's `@loader_path/../lib` rpath. A dev build has no
/// `.app`, so this returns `None` and the launcher leaves `ORT_DYLIB_PATH` for the developer to
/// set. Pure with respect to `exe_dir` (its only I/O is the existence probe), so it unit-tests on
/// any platform against a temp dir — hence not itself `#[cfg]`-gated (only the caller is).
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "x86_64")),
    allow(dead_code)
)]
fn bundled_ort_dylib(exe_dir: &Path) -> Option<PathBuf> {
    let candidate = exe_dir.join("../Resources/sherpa/lib").join(ORT_DYLIB_NAME);
    candidate.is_file().then_some(candidate)
}

/// On Intel macOS, point `ort`'s `load-dynamic` runtime at the bundled onnxruntime before any
/// `anvil-ai` session can be built, unless the developer already set `ORT_DYLIB_PATH` (their value
/// wins). A no-op outside a packaged `.app` (the dylib is absent, so [`bundled_ort_dylib`] returns
/// `None` and dev sets the var by hand). Called first thing in [`run`], before any thread exists.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn init_ort_dylib_path() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return; // an explicit developer/CI override wins
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    if let Some(dylib) = bundled_ort_dylib(dir) {
        tracing::info!(
            path = %dylib.display(),
            "Intel macOS: pointing ORT_DYLIB_PATH at the bundled onnxruntime (ort load-dynamic)"
        );
        // Edition 2021: `set_var` is safe. Called before Tauri, the audio thread, or the batch
        // queue spawn, so no other thread can be reading the environment concurrently. `ort` reads
        // ORT_DYLIB_PATH lazily when the first session is built, which is far later than here.
        std::env::set_var("ORT_DYLIB_PATH", &dylib);
    }
}

/// Basic build/runtime info surfaced to the UI (shown in About; also proves the IPC
/// bridge end-to-end during M0).
#[derive(Debug, Serialize)]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    chain_version: u32,
    platform: &'static str,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "ANVIL",
        version: env!("CARGO_PKG_VERSION"),
        chain_version: anvil_core::CHAIN_VERSION,
        platform: anvil_core::platform::current().name(),
    }
}

// --- OS "Open With" / file-association opens (FIX 1) ------------------------------------
//
// When macOS is asked to open a media file *with* Cleanroom — a Finder "Open With",
// double-clicking an associated file, or `open -a Cleanroom <file>` — the request arrives
// as an AppleEvent that Tauri surfaces as `RunEvent::Opened { urls }` (macOS/iOS/Android
// only; Windows passes files as argv instead — see the note in `run`). We route it to the
// SAME session-open path drag-drop uses: emit an `open-file` event to the webview, which
// calls `open_media` via `loadPath` and loads the file into the Master screen.
//
// The launch-with-file race: on a cold launch the event can fire before the webview has
// loaded and registered its listener, so opens are queued in `PendingOpens` until the
// frontend calls `frontend_ready`, then flushed in order. Once ready, later opens are
// emitted straight through.

/// Startup-race buffer for OS file-open requests (see the section comment above).
#[derive(Default)]
struct PendingOpens {
    inner: std::sync::Mutex<PendingOpensInner>,
}

#[derive(Default)]
struct PendingOpensInner {
    /// Set once the webview has mounted and subscribed to `open-file`.
    ready: bool,
    /// Absolute paths that arrived before `ready` — flushed in order by `frontend_ready`.
    queue: Vec<String>,
}

/// Emit one `open-file` event carrying an absolute path to the webview, which loads it into
/// the Master screen exactly as a drag-drop would (see `App.tsx`'s `onOpenFile`). The
/// `tracing::info!` here is the screenshot-free proof that an open reached the UI layer.
fn emit_open_file(app: &tauri::AppHandle, path: &str) {
    match app.emit("open-file", path) {
        Ok(()) => tracing::info!(path = %path, "open-with: emitted open-file to the webview"),
        Err(e) => tracing::warn!(path = %path, error = %e, "open-with: failed to emit open-file"),
    }
}

/// Called by the webview once it has mounted and subscribed to `open-file`. Marks the
/// frontend ready and flushes anything [`route_opened_urls`] queued during startup, so a
/// launch-with-file open lands in a live listener instead of being dropped. Idempotent:
/// after the first call the queue is empty and every later open is emitted straight
/// through by [`route_opened_urls`].
#[tauri::command]
fn frontend_ready(app: tauri::AppHandle, pending: State<'_, PendingOpens>) {
    let queued: Vec<String> = {
        let mut inner = pending.inner.lock().expect("PendingOpens mutex poisoned");
        inner.ready = true;
        std::mem::take(&mut inner.queue)
    };
    if !queued.is_empty() {
        tracing::info!(
            count = queued.len(),
            "open-with: frontend ready, flushing queued opens"
        );
    }
    for path in queued {
        emit_open_file(&app, &path);
    }
}

/// Emit `paths` to a ready webview, or queue them for [`frontend_ready`] if the frontend
/// hasn't mounted yet. The shared tail of both OS open mechanisms: macOS's
/// `RunEvent::Opened` and Windows's argv / single-instance file opens.
fn route_open_paths(app: &tauri::AppHandle, paths: Vec<String>) {
    if paths.is_empty() {
        return;
    }
    let pending = app.state::<PendingOpens>();
    {
        let mut inner = pending.inner.lock().expect("PendingOpens mutex poisoned");
        if !inner.ready {
            tracing::info!(
                count = paths.len(),
                "open-with: queued (frontend not ready yet)"
            );
            inner.queue.extend(paths);
            return;
        }
    }
    for path in &paths {
        emit_open_file(app, path);
    }
}

/// Pull openable file paths out of a process argv: skip `argv[0]` (the exe) and any `--flags`
/// (e.g. `--uninstall-cleanup`, handled in `main.rs`), keep only entries that exist as files,
/// made absolute. Windows delivers file-association / "Open With" opens as argv on a fresh
/// process (it has no `RunEvent::Opened`) — on a cold launch, and via single-instance when
/// the app is already running.
fn file_args_from_argv<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .skip(1)
        .filter_map(|a| {
            let a = a.as_ref();
            if a.starts_with('-') {
                return None;
            }
            let p = std::path::Path::new(a);
            if !p.is_file() {
                return None;
            }
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|c| c.join(p))
                    .unwrap_or_else(|_| p.to_path_buf())
            };
            Some(abs.to_string_lossy().into_owned())
        })
        .collect()
}

/// Convert the `file://` URLs from a macOS [`tauri::RunEvent::Opened`] into absolute paths
/// and either emit them to a ready webview or queue them for [`frontend_ready`]. Non-`file`
/// URLs (custom schemes) are logged and ignored — Cleanroom only opens local files.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
fn route_opened_urls(app: &tauri::AppHandle, urls: &[tauri::Url]) {
    let paths: Vec<String> = urls
        .iter()
        .filter_map(|u| match u.scheme() {
            "file" => match u.to_file_path() {
                Ok(p) => Some(p.to_string_lossy().into_owned()),
                Err(()) => {
                    tracing::warn!(url = %u, "open-with: could not convert file URL to a path");
                    None
                }
            },
            other => {
                tracing::warn!(url = %u, scheme = %other, "open-with: ignoring non-file URL");
                None
            }
        })
        .collect();
    if paths.is_empty() {
        return;
    }
    tracing::info!(?paths, "open-with: RunEvent::Opened received");
    route_open_paths(app, paths);
}

/// Route Tauri run-loop events. Only macOS/iOS/Android's `Opened` (the OS "open this file
/// with Cleanroom" event) is handled; everything else is a no-op. Kept as a free fn so the
/// `.run()` closure stays a one-liner and the platform `#[cfg]` lives in one place.
fn handle_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent) {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    if let tauri::RunEvent::Opened { urls } = event {
        route_opened_urls(app, urls);
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
    let _ = (app, event);
}

/// What the UI learns about a freshly opened file.
#[derive(Debug, Serialize)]
struct MediaSummary {
    /// Duration in seconds (from the internal 48 kHz frame count).
    duration_secs: f64,
    /// Source channel count (mono/stereo/…), preserved through the internal resample.
    channels: u32,
    /// Sample rate of the internal buffer — always [`anvil_core::INTERNAL_SAMPLE_RATE`];
    /// the peaks pyramid and playhead are in these frames, so the UI works in one domain.
    sample_rate: u32,
}

// `master` returns the engine's own `anvil_dsp::MasterReport` (analysis + before/after
// meters + module chips + Health Card rows), serialized straight to the S2 UI — that JSON
// is the M1 UI contract. Export's request/response types live in `export.rs`, which reads
// the `last_report`/`last_preset`/`last_preset_ref` fields below to build the compliance
// report from this run's real measurements.

/// Engine + waveform state, held in a Tauri `State`. The engine has its own interior
/// synchronisation (atomics + a dedicated audio thread), so it needs no outer lock; the
/// buffers/pyramids/A-B flag are swapped under their own locks.
pub(crate) struct AudioState {
    engine: PlaybackEngine,
    /// Filesystem path from the last `open_media` call — the master chain re-decodes from it.
    source_path: RwLock<Option<PathBuf>>,
    /// Decoded source buffer from the last `open_media` call.
    original: RwLock<Option<AudioBuffer>>,
    /// Peaks pyramid for `original`.
    peaks: RwLock<Option<PeaksPyramid>>,
    /// Mastered buffer from the last `master` call — `None` until then.
    processed: RwLock<Option<AudioBuffer>>,
    /// Peaks pyramid for `processed`.
    processed_peaks: RwLock<Option<PeaksPyramid>>,
    /// Which buffer the engine currently has loaded: `"original"` or `"processed"`.
    ab_source: RwLock<String>,
    /// Full report from the last `master` call — the compliance report's source of truth
    /// for before/after measurements and module decisions.
    last_report: RwLock<Option<MasterReport>>,
    /// The resolved preset the last `master` call actually rendered with.
    last_preset: RwLock<Option<Preset>>,
    /// The `preset_ref` string the last `master` call was invoked with (drives the
    /// compliance report's ACX section — see `anvil_project::ComplianceInput::preset_id`).
    last_preset_ref: RwLock<Option<String>>,
}

impl AudioState {
    fn new() -> Self {
        Self {
            engine: PlaybackEngine::new(),
            source_path: RwLock::new(None),
            original: RwLock::new(None),
            peaks: RwLock::new(None),
            processed: RwLock::new(None),
            processed_peaks: RwLock::new(None),
            ab_source: RwLock::new("original".to_string()),
            last_report: RwLock::new(None),
            last_preset: RwLock::new(None),
            last_preset_ref: RwLock::new(None),
        }
    }
}

/// Decode a file into the internal planar-f32 @ 48 kHz [`AudioBuffer`].
///
/// Delegates to `anvil_media` (symphonia + ffmpeg-sidecar fallback), which normalizes any
/// supported input — wav/flac/mp3/m4a/ogg natively and mp4/mov/mkv/webm through the
/// sidecar — to the internal 48 kHz rate, so the peaks pyramid and playhead share one
/// frame domain.
fn decode_to_internal(path: &Path) -> Result<AudioBuffer, String> {
    anvil_media::decode_to_buffer(path).map_err(|e| e.to_string())
}

/// Decode `path`, build its peaks pyramid, and load it into the playback engine. Clears
/// any previous `processed` buffer/A-B state — mastering is per-file — and any previous
/// transcript/cut-plan from the Transcript tab, which are equally per-file.
#[tauri::command]
fn open_media(
    path: String,
    state: State<'_, AudioState>,
    transcript_state: State<'_, transcript::TranscriptState>,
) -> Result<MediaSummary, String> {
    let buffer = decode_to_internal(Path::new(&path))?;
    let channels = buffer.channel_count() as u32;
    let sample_rate = buffer.sample_rate();
    let duration_secs = if sample_rate == 0 {
        0.0
    } else {
        buffer.frames() as f64 / sample_rate as f64
    };

    let pyramid = PeaksPyramid::build(&buffer);
    state.engine.load(&buffer).map_err(|e| e.to_string())?;
    *state.peaks.write().map_err(|_| "peaks lock poisoned")? = Some(pyramid);
    *state
        .source_path
        .write()
        .map_err(|_| "path lock poisoned")? = Some(PathBuf::from(&path));
    *state.original.write().map_err(|_| "audio lock poisoned")? = Some(buffer);
    *state.processed.write().map_err(|_| "audio lock poisoned")? = None;
    *state
        .processed_peaks
        .write()
        .map_err(|_| "peaks lock poisoned")? = None;
    *state.ab_source.write().map_err(|_| "ab lock poisoned")? = "original".to_string();
    *state
        .last_report
        .write()
        .map_err(|_| "report lock poisoned")? = None;
    *state
        .last_preset
        .write()
        .map_err(|_| "preset lock poisoned")? = None;
    *state
        .last_preset_ref
        .write()
        .map_err(|_| "preset lock poisoned")? = None;
    transcript_state.reset();

    tracing::info!(
        path = %path,
        duration_secs,
        channels,
        sample_rate,
        "open_media: loaded file into the session"
    );
    Ok(MediaSummary {
        duration_secs,
        channels,
        sample_rate,
    })
}

/// Min/max peaks over `[start_frame, end_frame)` as `bins` `[min, max]` pairs. Empty when
/// nothing is loaded. Frames are in the internal 48 kHz domain.
#[tauri::command]
fn get_peaks(
    start_frame: u64,
    end_frame: u64,
    bins: u32,
    state: State<'_, AudioState>,
) -> Result<Vec<[f32; 2]>, String> {
    let guard = state.peaks.read().map_err(|_| "peaks lock poisoned")?;
    let Some(pyramid) = guard.as_ref() else {
        return Ok(Vec::new());
    };
    let peaks = pyramid.peaks(start_frame as usize, end_frame as usize, bins as usize);
    Ok(peaks.into_iter().map(|(lo, hi)| [lo, hi]).collect())
}

/// Same as [`get_peaks`] but for the mastered buffer. Empty until `master` has run.
#[tauri::command]
fn get_processed_peaks(
    start_frame: u64,
    end_frame: u64,
    bins: u32,
    state: State<'_, AudioState>,
) -> Result<Vec<[f32; 2]>, String> {
    // INTEGRATION SEAM: wire to anvil_dsp::master — once `master` builds its pyramid from
    // a real processed render instead of a copy of `original`, this command needs no
    // change; it already just reads `state.processed_peaks`.
    let guard = state
        .processed_peaks
        .read()
        .map_err(|_| "peaks lock poisoned")?;
    let Some(pyramid) = guard.as_ref() else {
        return Ok(Vec::new());
    };
    let peaks = pyramid.peaks(start_frame as usize, end_frame as usize, bins as usize);
    Ok(peaks.into_iter().map(|(lo, hi)| [lo, hi]).collect())
}

#[tauri::command]
fn play(state: State<'_, AudioState>) {
    state.engine.play();
}

#[tauri::command]
fn pause(state: State<'_, AudioState>) {
    state.engine.pause();
}

#[tauri::command]
fn seek(frame: u64, state: State<'_, AudioState>) {
    state.engine.seek(frame);
}

/// Current playhead in internal 48 kHz frames (maps straight onto the waveform).
#[tauri::command]
fn playback_position(state: State<'_, AudioState>) -> u64 {
    state.engine.position()
}

/// Reload the engine with `source`'s buffer, preserving the playhead position and
/// play/pause state across the swap so A/B feels instant and sample-aligned.
///
/// `PlaybackEngine::load` resets the cursor to 0 (it's built for "open a new file"), so
/// this wrapper saves the position first and re-seeks after. That round-trip through the
/// caller's thread (resample/interleave the whole buffer again) is fine for a stub but is
/// not the real <50 ms production guarantee on long files — see the seam note on
/// [`set_ab`].
pub(crate) fn switch_ab(state: &AudioState, source: &str) -> Result<(), String> {
    let buffer = {
        let guard = match source {
            "original" => state.original.read(),
            "processed" => state.processed.read(),
            other => return Err(format!("unknown A/B source: {other}")),
        }
        .map_err(|_| "audio lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| format!("nothing loaded for A/B source \"{source}\""))?
            .clone()
    };

    let was_playing = state.engine.is_playing();
    let pos = state.engine.position();
    state.engine.load(&buffer).map_err(|e| e.to_string())?;
    state.engine.seek(pos);
    if was_playing {
        state.engine.play();
    }
    Ok(())
}

/// Switch the engine between the original and mastered buffers. `source` is
/// `"original"` or `"processed"`.
#[tauri::command]
fn set_ab(source: String, state: State<'_, AudioState>) -> Result<(), String> {
    // INTEGRATION SEAM: wire to anvil_dsp::master — this already does a real,
    // position-preserving buffer swap; the seam is the *production* <50 ms guarantee
    // (04 §Per-feature acceptance criteria: "A/B toggle latency < 50 ms"), which on long
    // files wants the audio thread holding both buffers and switching a pointer rather
    // than resampling/interleaving on every toggle as this stub does.
    switch_ab(&state, &source)?;
    *state.ab_source.write().map_err(|_| "ab lock poisoned")? = source;
    Ok(())
}

/// Resolve `master`'s `preset`/`tier` params to an `anvil_project::Preset`: decode
/// `preset` through the shared [`presets::resolve_preset_ref`] contract (shipped id or
/// `user:<uuid>`), then apply the tier selector on top — the Master tab's Tier control
/// always wins over whatever tier the preset itself carries, matching the pre-M2 stub's
/// behavior.
fn resolve_master_preset(preset: &str, tier: &str, presets_dir: &Path) -> Result<Preset, String> {
    let mut resolved = presets::resolve_preset_ref(preset, presets_dir)?;
    resolved.tier = presets::parse_tier(tier);
    Ok(resolved)
}

/// Run the one-click Master chain on the currently-open file at `preset`/`tier`, returning
/// the engine's full [`MasterReport`]. Re-decodes from the source path, runs the real
/// `anvil_dsp` analysis + auto-decision + chain, then loads the mastered buffer so A/B and
/// `get_processed_peaks` work immediately. Also stashes the report/preset for Export's
/// compliance-report checkbox.
#[tauri::command]
fn master(
    preset: String,
    tier: String,
    state: State<'_, AudioState>,
    presets: State<'_, presets::PresetsState>,
) -> Result<MasterReport, String> {
    let path = {
        let guard = state.source_path.read().map_err(|_| "path lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before mastering".to_string())?
            .clone()
    };
    let resolved = resolve_master_preset(&preset, &tier, presets.dir())?;
    let result = anvil_dsp::master(&path, &resolved, resolved.tier).map_err(|e| e.to_string())?;

    let pyramid = PeaksPyramid::build(&result.audio);
    *state.processed.write().map_err(|_| "audio lock poisoned")? = Some(result.audio);
    *state
        .processed_peaks
        .write()
        .map_err(|_| "peaks lock poisoned")? = Some(pyramid);
    switch_ab(&state, "processed")?;
    *state.ab_source.write().map_err(|_| "ab lock poisoned")? = "processed".to_string();
    *state
        .last_report
        .write()
        .map_err(|_| "report lock poisoned")? = Some(result.report.clone());
    *state
        .last_preset
        .write()
        .map_err(|_| "preset lock poisoned")? = Some(resolved);
    *state
        .last_preset_ref
        .write()
        .map_err(|_| "preset lock poisoned")? = Some(preset);

    Ok(result.report)
}

/// Poll the batch queue + watch rules and re-broadcast their state as `batch://progress` /
/// `watch://status` events, so the Batch/Watch screens update live without polling
/// themselves. `anvil_batch` exposes snapshots, not a push channel, so a short poll here is
/// the simplest honest bridge — see `batch.rs`/`watch.rs` module docs.
fn spawn_progress_poller(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(400));
        if let Some(batch_state) = app.try_state::<batch::BatchState>() {
            let _ = app.emit("batch://progress", batch_state.queue.snapshot());
            batch::prune_recovery(&batch_state.queue);
        }
        if let Some(watch_state) = app.try_state::<watch::WatchState>() {
            let _ = app.emit("watch://status", watch_state.service.list_rules());
        }
    });
}

/// Run by [`uninstall_cleanup`] and `main.rs`'s CLI check before Tauri boots (a bare
/// `--uninstall-cleanup` run never needs a log file), and by [`run`] for the normal app
/// session. Local-only: stderr plus a rolling daily file under the platform log dir
/// (`diagnostics::log_dir`) so `export_diagnostics` has something real to zip — never a
/// network sink, never telemetry (02 §Privacy). Returns the `WorkerGuard` the caller must
/// keep alive for the process lifetime, or the file writer silently stops flushing.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    // `EnvFilter::from_default_env()` — what the pre-M5 scaffold used — falls back to
    // "nothing enabled" when `RUST_LOG` isn't set, which is every real user's launch (they
    // don't set env vars). That made `export_diagnostics`'s log files silently empty in
    // practice; verified empirically (built the release exe, ran it, checked the file).
    // Default to "info" instead — still fully overridable via `RUST_LOG` for anyone who
    // wants more/less — so a diagnostics zip actually has something in it.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let log_dir = diagnostics::log_dir();
    if std::fs::create_dir_all(&log_dir).is_err() {
        // Best-effort: fall back to stderr-only rather than fail the whole app over a
        // logs directory we couldn't create.
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        return None;
    }
    let file_appender = tracing_appender::rolling::daily(&log_dir, "anvil.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let writer = std::io::stderr.and(non_blocking);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .try_init();
    Some(guard)
}

/// Run from `main.rs` when launched as `<exe> --uninstall-cleanup` by the NSIS
/// uninstaller's `NSIS_HOOK_PREUNINSTALL` (`installer-hooks.nsh`). Removes every HKCU key
/// `anvil_core::platform` may have written — Explorer context menu, "Open with" list,
/// autostart — regardless of whether the user ever turned them on from Settings; each
/// unregister call is idempotent, so running this unconditionally on every uninstall is
/// safe even when nothing was ever registered. Never opens a window, never touches
/// anything outside ANVIL's own keys (see `platform/windows.rs`'s module doc).
pub fn uninstall_cleanup() {
    // NOT `let _ = ...` — that drops the `WorkerGuard` immediately, which can tear down
    // the non-blocking writer before the `tracing::info!`/`warn!` calls below ever reach
    // disk. Binding it keeps the writer alive for this function's whole body.
    let _log_guard = init_logging();
    tracing::info!("uninstall cleanup: unregistering shell integration");
    let platform = anvil_core::platform::current();
    if let Err(e) = platform.unregister_context_menu() {
        tracing::warn!("uninstall cleanup: context menu unregister failed: {e}");
    }
    if let Err(e) = platform.unregister_file_associations() {
        tracing::warn!("uninstall cleanup: file association unregister failed: {e}");
    }
    if let Err(e) = platform.set_autostart(false) {
        tracing::warn!("uninstall cleanup: autostart disable failed: {e}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _log_guard = init_logging();

    // On Intel macOS, `ort` is load-dynamic (crates/anvil-ai/Cargo.toml) — resolve the bundled
    // onnxruntime into ORT_DYLIB_PATH before any `anvil-ai` session is built (the master chain's
    // DeepFilterNet3 pass is the first to build one). First thing after logging, before any thread
    // is spawned, so the env write is race-free; a no-op on every other target and in dev.
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    init_ort_dylib_path();

    let batch_state = batch::BatchState::new().expect("failed to start the batch queue");
    let watch_state = watch::WatchState::new(std::sync::Arc::clone(&batch_state.queue));

    tauri::Builder::default()
        // Must be the first plugin. Routes a *second* launch's file argv (Windows "Open
        // With" / double-click while the app is already running) into the running instance
        // instead of spawning a new window — same open-file path the macOS handler uses.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            route_open_paths(app, file_args_from_argv(argv));
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // M5: Tauri updater against GitHub Releases (`tauri.conf.json`'s
        // `plugins.updater`). The endpoint org/repo and signing pubkey are both owner
        // TODOs there (the repo doesn't exist yet) — registering the plugin here is real
        // wiring either way; `check()` just returns a clear error until those are filled
        // in, rather than the app silently pretending updates work.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(AudioState::new())
        .manage(PendingOpens::default())
        .manage(presets::PresetsState::new())
        .manage(batch_state)
        .manage(watch_state)
        .manage(models::ModelsState::new())
        .manage(transcript::TranscriptState::new())
        .manage(multitrack::MultitrackState::new())
        .manage(guard::GuardState::new())
        .setup(|app| {
            spawn_progress_poller(app.handle().clone());
            // Windows cold "Open With": the file arrives in THIS process's argv. Queue it
            // (the frontend isn't up yet) so `frontend_ready` flushes it into the UI. No-op
            // on macOS, where opens arrive as `RunEvent::Opened` instead of argv.
            route_open_paths(app.handle(), file_args_from_argv(std::env::args()));
            // Upgrade hygiene: strip any orphaned pre-rename ANVIL shell integration so an
            // in-place upgrade from an old ANVIL install doesn't leave dead registry entries.
            anvil_core::platform::remove_legacy_windows_identity();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            frontend_ready,
            open_media,
            get_peaks,
            get_processed_peaks,
            play,
            pause,
            seek,
            playback_position,
            set_ab,
            master,
            export::export_outputs,
            presets::presets_list,
            presets::presets_duplicate,
            presets::presets_update,
            presets::presets_delete,
            presets::presets_import,
            presets::presets_export,
            batch::batch_submit_files,
            batch::batch_submit_folder,
            batch::batch_snapshot,
            batch::batch_overall_progress,
            batch::batch_cancel,
            batch::batch_cancel_all,
            batch::batch_pause,
            batch::batch_resume,
            batch::batch_is_paused,
            batch::batch_reorder,
            batch::batch_remove,
            batch::batch_retry_failed,
            batch::batch_path_kind,
            watch::watch_list_rules,
            watch::watch_add_rule,
            watch::watch_remove_rule,
            watch::watch_set_enabled,
            watch::watch_retry_unreachable,
            models::models_list,
            models::download_model,
            models::download_model_cancel,
            transcript::transcribe,
            transcript::diarize,
            transcript::plan_cuts,
            transcript::apply_cuts,
            transcript::export_transcript,
            transcript::write_text_file,
            metadata::metadata_read,
            metadata::metadata_write,
            shownotes::generate_shownotes,
            multitrack::multitrack_load_tracks,
            multitrack::multitrack_list_tracks,
            multitrack::multitrack_get_peaks,
            multitrack::multitrack_update_track,
            multitrack::multitrack_remove_track,
            multitrack::multitrack_clear,
            multitrack::multitrack_align,
            multitrack::multitrack_undo_align,
            multitrack::multitrack_get_alignment,
            multitrack::multitrack_mix,
            clip_studio::clip_studio_render,
            guard::guard_list_devices,
            guard::guard_start,
            guard::guard_stop,
            guard::guard_meter,
            guard::guard_clap_test,
            batch::batch_check_recovery,
            batch::batch_dismiss_recovery,
            diagnostics::export_diagnostics,
            settings::settings_set_context_menu,
            settings::settings_set_file_associations,
            settings::settings_set_autostart,
            settings::settings_get_integration_status,
        ])
        // `.build().run(cb)` rather than `.run(ctx)` so the run-loop callback can handle
        // macOS's `RunEvent::Opened`. Windows delivers file-association / "Open With" opens
        // as argv on a fresh process (Tauri only emits `Opened` on macOS/iOS/Android): the
        // single-instance plugin routes an already-running app's second-launch argv, and the
        // `setup` argv parse handles a cold launch — both into the same `route_open_paths`
        // path. `main.rs` still intercepts `--uninstall-cleanup` before any of this.
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| handle_run_event(app_handle, &event));
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_core::INTERNAL_SAMPLE_RATE;

    /// The bundled-onnxruntime resolver (Intel-macOS ORT_DYLIB_PATH wiring): it points at
    /// `../Resources/sherpa/lib/<dylib>` relative to the exe dir — the packaged `.app` layout — and
    /// only reports a hit when the file is actually there, so a dev build (no `.app`) yields `None`
    /// and the launcher leaves the env untouched. Runs on every platform (the helper is not
    /// `#[cfg]`-gated), so it is real coverage on the arm64 host, not just under a cross target.
    #[test]
    fn bundled_ort_dylib_resolves_the_app_resources_copy() {
        let tmp = std::env::temp_dir().join(format!("anvil-ort-dylib-{}", std::process::id()));
        let macos = tmp.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();

        // No dylib staged yet (a dev build / bare exe): the resolver declines.
        assert_eq!(bundled_ort_dylib(&macos), None);

        // Stage it exactly where the sherpa lane puts it, and the resolver finds precisely that.
        let lib = tmp
            .join("Contents")
            .join("Resources")
            .join("sherpa")
            .join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join(ORT_DYLIB_NAME),
            b"\xcf\xfa\xed\xfe not a real dylib",
        )
        .unwrap();

        let got = bundled_ort_dylib(&macos).expect("resolver finds the staged dylib");
        assert!(got.is_file(), "resolved path must exist: {got:?}");
        assert!(
            got.ends_with("Resources/sherpa/lib/libonnxruntime.1.17.1.dylib"),
            "must resolve the Resources/sherpa/lib copy, got {got:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Write a 16-bit stereo wav (L = +0.5, R = -0.5) at 48 kHz and decode it back
    /// through the real `anvil_media` decoder (exercises the wired integration seam).
    #[test]
    fn decodes_stereo_wav_to_internal_buffer() {
        let path = std::env::temp_dir().join("anvil_seam_test.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: INTERNAL_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..1000 {
            writer.write_sample(16_384i16).unwrap(); // +0.5
            writer.write_sample(-16_384i16).unwrap(); // -0.5
        }
        writer.finalize().unwrap();

        let buf = decode_to_internal(&path).unwrap();
        assert_eq!(buf.channel_count(), 2);
        assert_eq!(buf.sample_rate(), INTERNAL_SAMPLE_RATE);
        assert_eq!(buf.frames(), 1000); // already 48 kHz → no resample
        assert!((buf.channel(0)[0] - 0.5).abs() < 1e-3);
        assert!((buf.channel(1)[0] + 0.5).abs() < 1e-3);

        let _ = std::fs::remove_file(&path);
    }

    /// Preset ids sent by the UI resolve to the right loudness targets, tiers, and
    /// ceilings, with the Tier control always overriding the preset's own tier.
    #[test]
    fn resolve_master_preset_maps_targets_and_tiers() {
        let p = resolve_master_preset("podcast_stereo", "standard", Path::new(".")).unwrap();
        assert_eq!(p.target_lufs, -16.0);
        assert_eq!(p.tier, anvil_project::Tier::Standard);

        let b = resolve_master_preset("broadcast_ebu", "studio", Path::new(".")).unwrap();
        assert_eq!(b.target_lufs, -23.0);
        assert_eq!(b.tier, anvil_project::Tier::Studio);

        let a = resolve_master_preset("audiobook_acx", "fast", Path::new(".")).unwrap();
        assert_eq!(a.true_peak_ceiling_dbtp, -3.0);
        assert_eq!(a.tier, anvil_project::Tier::Fast);
    }

    /// An unknown shipped id is a clear error, not a silent fallback (honesty over
    /// faking a preset the user didn't ask for).
    #[test]
    fn resolve_master_preset_errors_on_unknown_id() {
        let err = resolve_master_preset("does_not_exist", "standard", Path::new(".")).unwrap_err();
        assert!(err.contains("does_not_exist"));
    }
}
