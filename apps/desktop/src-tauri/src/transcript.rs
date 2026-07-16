//! Transcript tab commands (04 §S2 Transcript tab, M3): transcribe, plan silence/filler
//! cuts, apply the accepted ones, and format the result for export. `transcribe` runs the
//! real `anvil_asr` whisper.cpp sidecar, `plan_cuts` runs the real `anvil_cut::plan` engine
//! against real ASR words + a real silence analysis, and `apply_cuts` renders the accepted
//! cuts with `anvil_cut::apply`'s crossfaded excision (a genuinely shorter render, not just a
//! silenced-in-place stub). `export_transcript` was already real (it only formats whatever
//! `TranscriptState` holds) and needs no change here.
//!
//! Wire types (`Word`, `TranscriptSegment`, `Transcript`, `CutKind`, `Cut`, `CutPlan`) stay
//! local to this module rather than re-exporting `anvil_asr`/`anvil_cut`'s types directly —
//! they *are* the 04 §Interfaces JSON contract the UI (`src/api.ts`) depends on, and keeping
//! them separate means an engine-side field rename can't silently break the wire shape.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anvil_audio::PeaksPyramid;
use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::AudioState;

/// One ASR word with its own timing/confidence (04 §Interfaces contract). `speaker` is the
/// diarized speaker id (see [`diarize`]) or `None` before diarization; `#[serde(default)]`
/// keeps transcripts written before diarization existed deserializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub confidence: f32,
    #[serde(default)]
    pub speaker: Option<u32>,
}

impl From<anvil_asr::Word> for Word {
    fn from(w: anvil_asr::Word) -> Self {
        Word {
            text: w.text,
            start: w.start,
            end: w.end,
            confidence: w.confidence,
            speaker: w.speaker,
        }
    }
}

/// One ASR segment (a sentence-ish grouping of words) — the unit subtitle exports work in.
/// `speaker` is the segment's dominant diarized speaker, or `None` before diarization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub speaker: Option<u32>,
}

impl From<anvil_asr::Segment> for TranscriptSegment {
    fn from(s: anvil_asr::Segment) -> Self {
        TranscriptSegment {
            text: s.text,
            start: s.start,
            end: s.end,
            speaker: s.speaker,
        }
    }
}

/// The Transcript tab's wire contract: detected language, word-level timestamps (for
/// playback-follow + click-to-seek), and sentence-level segments (for subtitle exports).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    pub language: String,
    pub words: Vec<Word>,
    pub segments: Vec<TranscriptSegment>,
}

impl From<anvil_asr::Transcript> for Transcript {
    fn from(t: anvil_asr::Transcript) -> Self {
        Transcript {
            language: t.language,
            words: t.words.into_iter().map(Word::from).collect(),
            segments: t
                .segments
                .into_iter()
                .map(TranscriptSegment::from)
                .collect(),
        }
    }
}

/// "silence" | "filler" — matches the contract's lowercase string values on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CutKind {
    Silence,
    Filler,
}

impl From<anvil_cut::CutKind> for CutKind {
    fn from(k: anvil_cut::CutKind) -> Self {
        match k {
            anvil_cut::CutKind::Silence => CutKind::Silence,
            anvil_cut::CutKind::Filler => CutKind::Filler,
        }
    }
}

/// One candidate cut in the filler/silence review list (04 §S2 "each cut: play-in-context
/// button, accept/reject").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cut {
    pub start: f64,
    pub end: f64,
    pub kind: CutKind,
    pub label: String,
    pub accepted: bool,
}

impl From<&anvil_cut::Cut> for Cut {
    fn from(c: &anvil_cut::Cut) -> Self {
        Cut {
            start: c.start,
            end: c.end,
            kind: c.kind.into(),
            label: c.label.clone(),
            accepted: c.accepted,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CutPlan {
    pub cuts: Vec<Cut>,
}

impl From<&anvil_cut::CutPlan> for CutPlan {
    fn from(p: &anvil_cut::CutPlan) -> Self {
        CutPlan {
            cuts: p.cuts.iter().map(Cut::from).collect(),
        }
    }
}

/// Last transcribe/plan results, held so `export_transcript` and `apply_cuts` have
/// something to act on without the UI re-sending the whole payload. Reset by `open_media`
/// (lib.rs) when a new file loads, same as `AudioState`'s mastering fields.
///
/// `last_plan` holds the real `anvil_cut::CutPlan` (not the wire [`CutPlan`]) because
/// `apply_cuts` needs its `source_duration` field to build the EDL's trailing kept segment —
/// the wire type only carries what the UI needs to render the review list.
#[derive(Default)]
pub struct TranscriptState {
    last_transcript: RwLock<Option<Transcript>>,
    last_plan: RwLock<Option<anvil_cut::CutPlan>>,
}

impl TranscriptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears both fields for a freshly opened file (called from `lib.rs::open_media`).
    pub fn reset(&self) {
        if let Ok(mut t) = self.last_transcript.write() {
            *t = None;
        }
        if let Ok(mut p) = self.last_plan.write() {
            *p = None;
        }
    }

    /// The last `transcribe` result, if any — read by `clip_studio::clip_studio_render` to
    /// find caption text for the selected range. `None` before the first transcribe (or
    /// after a fresh file reset it); Clip Studio renders without captions in that case
    /// rather than erroring, since captions are optional (04 §Clip Studio).
    pub fn snapshot(&self) -> Option<Transcript> {
        self.last_transcript.read().ok().and_then(|g| g.clone())
    }
}

// ---- transcribe ----------------------------------------------------------------------

/// Resolve the Models screen's ASR pack id (e.g. `"whisper-small"`, from `models.rs`'s
/// `ModelPack::id`) — or a raw `anvil_asr` catalog id (e.g. `"small.en"`) — to an installed
/// model's `.bin` path.
///
/// Tries three things in order: the id verbatim against `anvil_asr::locate_model` (so a
/// caller that already knows the `anvil_asr` id works), the id with a `"whisper-"` prefix
/// stripped against the same search (the multilingual variant the Models screen downloads,
/// `ggml-<size>.bin`, no `.en`), and finally `models_dir` — `models::ModelsState::dir()` —
/// directly. `anvil_asr::models_dirs` now searches that per-user config dir itself (its first
/// entry after the `ANVIL_WHISPER_MODELS_DIR` override — see that function), so the first two
/// probes already find a pack downloaded here; the explicit `models_dir` check stays as a
/// belt-and-suspenders for the exact directory this app installs into. Returns `None` — never
/// downloads — if nothing installed matches any of the three.
fn resolve_transcribe_model(model: &str, models_dir: &Path) -> Option<PathBuf> {
    if let Some(path) = anvil_asr::locate_model(model) {
        return Some(path);
    }
    if let Some(mapped) = model.strip_prefix("whisper-") {
        if let Some(path) = anvil_asr::locate_model(mapped) {
            return Some(path);
        }
    }
    let filename = crate::models::asr_ggml_filename(model)?;
    let candidate = models_dir.join(filename);
    candidate.is_file().then_some(candidate)
}

/// A `-of`-unique staging path in the system temp dir (whisper.cpp reads a file, not stdin).
fn unique_temp_wav_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "anvil-desktop-asr-{}-{nanos}.wav",
        std::process::id()
    ))
}

/// Downmix to mono and resample to 16 kHz (what whisper.cpp wants), then write 16-bit PCM
/// WAV. Mirrors `anvil-cli`'s `cmd_transcribe`/`write_wav_16k_mono` staging step exactly;
/// duplicated here rather than shared because `apps/desktop` doesn't depend on `anvil-cli`
/// (and this crate is out of scope for a shared-helper refactor per the M3 UI-wiring brief).
fn write_wav_16k_mono(path: &Path, audio: &AudioBuffer) -> Result<(), String> {
    let channels = audio.channel_count().max(1);
    let frames = audio.frames();
    let mut mono = vec![0.0f32; frames];
    for c in 0..channels {
        for (i, &s) in audio.channel(c).iter().enumerate() {
            mono[i] += s;
        }
    }
    let inv = 1.0 / channels as f32;
    for s in &mut mono {
        *s *= inv;
    }
    let src_rate = audio.sample_rate().max(1) as f64;
    let dst_rate = 16_000.0f64;
    let out_len = ((mono.len() as f64) * dst_rate / src_rate).round() as usize;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for i in 0..out_len {
        let pos = i as f64 * src_rate / dst_rate;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = mono.get(idx).copied().unwrap_or(0.0);
        let b = mono.get(idx + 1).copied().unwrap_or(a);
        let s = a + (b - a) * frac;
        writer
            .write_sample((s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
            .map_err(|e| e.to_string())?;
    }
    writer.finalize().map_err(|e| e.to_string())
}

/// Transcribe the currently open file with `model` — either a Models-screen ASR pack id
/// (`"whisper-small"`) or a raw `anvil_asr` catalog id (`"small.en"`). Stages a 16 kHz mono
/// WAV from the open buffer (whisper.cpp's preferred input — see `write_wav_16k_mono`), runs
/// the real `anvil_asr::transcribe` whisper.cpp sidecar, and caches the result for
/// `plan_cuts`/`export_transcript`.
#[tauri::command]
pub fn transcribe(
    model: String,
    state: State<'_, AudioState>,
    tstate: State<'_, TranscriptState>,
    mstate: State<'_, crate::models::ModelsState>,
) -> Result<Transcript, String> {
    let buffer = {
        let guard = state.original.read().map_err(|_| "audio lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before transcribing".to_string())?
            .clone()
    };

    let model_path = resolve_transcribe_model(&model, mstate.dir()).ok_or_else(|| {
        format!("the \"{model}\" model isn't installed — install it in Models, then try again.")
    })?;

    let staged = unique_temp_wav_path();
    write_wav_16k_mono(&staged, &buffer)
        .map_err(|e| format!("could not stage audio for transcription: {e}"))?;

    let opts = anvil_asr::TranscribeOptions {
        model: Some(model_path),
        ..anvil_asr::TranscribeOptions::default()
    };
    let result = anvil_asr::transcribe(&staged, &opts);
    let _ = std::fs::remove_file(&staged);
    let asr_transcript = result.map_err(|e| format!("transcribe failed: {e}"))?;
    let transcript = Transcript::from(asr_transcript);

    *tstate
        .last_transcript
        .write()
        .map_err(|_| "transcript lock poisoned")? = Some(transcript.clone());
    // A fresh transcript invalidates any cut plan computed against the old word timings.
    *tstate
        .last_plan
        .write()
        .map_err(|_| "cut plan lock poisoned")? = None;

    Ok(transcript)
}

// ---- diarize --------------------------------------------------------------------------

/// One speaker in a diarized recording, on the wire: a dense id and a display label
/// (`"Speaker 1"`, …), which the UI may rename. Mirrors [`anvil_asr::Speaker`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerWire {
    pub id: u32,
    pub label: String,
}

impl From<&anvil_asr::Speaker> for SpeakerWire {
    fn from(s: &anvil_asr::Speaker) -> Self {
        SpeakerWire {
            id: s.id,
            label: s.label.clone(),
        }
    }
}

/// The [`diarize`] result: the transcript with a `speaker` stamped on every word/segment, and
/// the cast list for the UI's legend + label colours.
#[derive(Debug, Clone, Serialize)]
pub struct DiarizeResult {
    pub transcript: Transcript,
    pub speakers: Vec<SpeakerWire>,
}

/// Rebuild an `anvil_asr::Transcript` from the wire type so [`anvil_asr::assign_speakers`] can
/// stamp speakers back onto it. The inverse (`Transcript::from`) already exists.
fn to_asr_transcript(t: &Transcript) -> anvil_asr::Transcript {
    anvil_asr::Transcript {
        language: t.language.clone(),
        words: t
            .words
            .iter()
            .map(|w| anvil_asr::Word {
                text: w.text.clone(),
                start: w.start,
                end: w.end,
                confidence: w.confidence,
                speaker: w.speaker,
            })
            .collect(),
        segments: t
            .segments
            .iter()
            .map(|s| anvil_asr::Segment {
                text: s.text.clone(),
                start: s.start,
                end: s.end,
                speaker: s.speaker,
            })
            .collect(),
    }
}

/// Diarize the currently open file (04 §S2 "speaker labels") and map the speaker turns onto
/// the last `transcribe` result, stamping every word and segment with who said it.
///
/// Stages the same 16 kHz mono WAV `transcribe` does (so the sherpa sidecar needs no ffmpeg
/// resample — it gets exactly the format it wants), runs `anvil_asr::diarize`, then
/// `assign_speakers` to merge the turns onto the transcript, and caches the diarized
/// transcript so exports/clip captions see the speaker tags too. `num_speakers` forces an
/// exact count when the user knows it (a host+guest interview → `Some(2)`); `None` auto-detects.
///
/// Requires a transcript first — speaker labels are shown *on* the words. When the sherpa
/// binary or its ONNX models are absent, returns the same kind of clean, actionable error
/// `transcribe` gives (the sidecar is provisioned separately); it never crashes or fabricates.
#[tauri::command]
pub fn diarize(
    num_speakers: Option<usize>,
    state: State<'_, AudioState>,
    tstate: State<'_, TranscriptState>,
) -> Result<DiarizeResult, String> {
    let transcript = tstate
        .snapshot()
        .ok_or_else(|| "transcribe the file before identifying speakers".to_string())?;

    let buffer = {
        let guard = state.original.read().map_err(|_| "audio lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before identifying speakers".to_string())?
            .clone()
    };

    let staged = unique_temp_wav_path();
    write_wav_16k_mono(&staged, &buffer)
        .map_err(|e| format!("could not stage audio for diarization: {e}"))?;

    let opts = anvil_asr::DiarizeOptions {
        num_speakers: num_speakers.filter(|&n| n > 0),
        ..anvil_asr::DiarizeOptions::default()
    };
    let result = anvil_asr::diarize(&staged, &opts);
    let _ = std::fs::remove_file(&staged);
    let diarization = result.map_err(|e| format!("could not identify speakers: {e}"))?;

    let mut asr_transcript = to_asr_transcript(&transcript);
    anvil_asr::assign_speakers(&mut asr_transcript, &diarization);
    let diarized = Transcript::from(asr_transcript);

    *tstate
        .last_transcript
        .write()
        .map_err(|_| "transcript lock poisoned")? = Some(diarized.clone());

    Ok(DiarizeResult {
        transcript: diarized,
        speakers: diarization.speakers.iter().map(SpeakerWire::from).collect(),
    })
}

// ---- plan_cuts / apply_cuts -----------------------------------------------------------

/// Map the Transcript tab's `mode` control to real `anvil_cut::CutOptions`. `mode` is
/// `"silence"`, `"filler"`, or `"both"` (04 §S2's three-way "Look for" selector — anything
/// else, including empty, behaves like `"both"`); a `"aggressive"` substring (e.g. a future
/// `"both-aggressive"` control) additionally switches on `anvil_cut`'s aggressive hedge
/// lexicon (`you know` / `like`) without changing which kinds are cut.
fn cut_options_for_mode(mode: &str) -> anvil_cut::CutOptions {
    let lower = mode.to_ascii_lowercase();
    let aggressive = lower.contains("aggressive");
    let kind = lower
        .replace("aggressive", "")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string();
    let (cut_silence, cut_fillers) = match kind.as_str() {
        "silence" => (true, false),
        "filler" => (false, true),
        _ => (true, true), // "", "both", "aggressive" alone, or anything unrecognized
    };
    anvil_cut::CutOptions {
        aggressive,
        cut_silence,
        cut_fillers,
        ..anvil_cut::CutOptions::default()
    }
}

/// Plan silence/filler cuts for the currently open file: real VAD silence runs from
/// `anvil_dsp::analyze_buffer`, real filler cuts from the last `transcribe`'s word timestamps
/// (empty — silence-only — if nothing has been transcribed yet), via `anvil_cut::plan`.
#[tauri::command]
pub fn plan_cuts(
    mode: String,
    state: State<'_, AudioState>,
    tstate: State<'_, TranscriptState>,
) -> Result<CutPlan, String> {
    let buffer = {
        let guard = state.original.read().map_err(|_| "audio lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before planning cuts".to_string())?
            .clone()
    };
    let duration = buffer.frames() as f64 / f64::from(buffer.sample_rate().max(1));

    let last_transcript = tstate
        .last_transcript
        .read()
        .map_err(|_| "transcript lock poisoned")?
        .clone();
    let language = last_transcript
        .as_ref()
        .map(|t| t.language.clone())
        .unwrap_or_default();
    let words: Vec<anvil_cut::Word> = last_transcript
        .as_ref()
        .map(|t| {
            t.words
                .iter()
                .map(|w| anvil_cut::Word {
                    text: w.text.clone(),
                    start: w.start,
                    end: w.end,
                    confidence: w.confidence,
                })
                .collect()
        })
        .unwrap_or_default();
    // `anvil_cut::plan` only reads `transcript.language` (the filler lexicon selector) — see
    // that crate's docs on why the word slice is a separate parameter.
    let transcript_for_cut = anvil_asr::Transcript {
        language,
        words: Vec::new(),
        segments: Vec::new(),
    };

    let analysis = anvil_dsp::analyze_buffer(&buffer);
    let silence =
        anvil_cut::SilenceInput::from_runs(analysis.silence_runs.iter().map(|r| (r.start, r.end)));

    let opts = cut_options_for_mode(&mode);
    let plan = anvil_cut::plan(&transcript_for_cut, &words, &silence, &opts)
        .with_source_duration(duration);

    let wire_plan = CutPlan::from(&plan);
    *tstate
        .last_plan
        .write()
        .map_err(|_| "cut plan lock poisoned")? = Some(plan);
    Ok(wire_plan)
}

/// Apply the accepted cuts from the last `plan_cuts` result: builds the accepted-only EDL
/// (`anvil_cut::to_edl`) and renders it with a real crossfaded excision (`anvil_cut::apply`)
/// against the source buffer — the output is genuinely shorter, not just silenced in place.
/// Swaps the render in as the A/B "processed" take (rebuilding its peaks pyramid) and reloads
/// the playback engine, same as `master`. `accepted_indices` is the complete set of cut
/// indices to accept — anything not listed is (re)marked rejected.
#[tauri::command]
pub fn apply_cuts(
    accepted_indices: Vec<usize>,
    state: State<'_, AudioState>,
    tstate: State<'_, TranscriptState>,
) -> Result<(), String> {
    let accepted: std::collections::HashSet<usize> = accepted_indices.into_iter().collect();
    let plan = {
        let mut guard = tstate
            .last_plan
            .write()
            .map_err(|_| "cut plan lock poisoned")?;
        let plan = guard
            .as_mut()
            .ok_or_else(|| "plan cuts before applying them".to_string())?;
        for (i, cut) in plan.cuts.iter_mut().enumerate() {
            cut.accepted = accepted.contains(&i);
        }
        plan.clone()
    };

    let original = state
        .original
        .read()
        .map_err(|_| "audio lock poisoned")?
        .clone()
        .ok_or_else(|| "open a file before applying cuts".to_string())?;

    let edl = anvil_cut::to_edl(&plan);
    let rendered = anvil_cut::apply(&edl, &original);

    let pyramid = PeaksPyramid::build(&rendered);
    *state.processed.write().map_err(|_| "audio lock poisoned")? = Some(rendered);
    *state
        .processed_peaks
        .write()
        .map_err(|_| "peaks lock poisoned")? = Some(pyramid);
    crate::switch_ab(&state, "processed")?;
    *state.ab_source.write().map_err(|_| "ab lock poisoned")? = "processed".to_string();
    Ok(())
}

// ---- export_transcript -----------------------------------------------------------------

fn clock(secs: f64, ms_sep: char) -> String {
    let total_ms = (secs.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02}{ms_sep}{ms:03}")
}

fn to_srt(t: &Transcript) -> String {
    let mut out = String::new();
    for (i, seg) in t.segments.iter().enumerate() {
        out.push_str(&format!("{}\n", i + 1));
        out.push_str(&format!(
            "{} --> {}\n",
            clock(seg.start, ','),
            clock(seg.end, ',')
        ));
        out.push_str(seg.text.trim());
        out.push_str("\n\n");
    }
    out
}

fn to_vtt(t: &Transcript) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for seg in &t.segments {
        out.push_str(&format!(
            "{} --> {}\n",
            clock(seg.start, '.'),
            clock(seg.end, '.')
        ));
        out.push_str(seg.text.trim());
        out.push_str("\n\n");
    }
    out
}

fn to_txt(t: &Transcript) -> String {
    t.segments
        .iter()
        .map(|s| s.text.trim())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_export(t: &Transcript, format: &str) -> Result<String, String> {
    match format.to_ascii_lowercase().as_str() {
        "srt" => Ok(to_srt(t)),
        "vtt" => Ok(to_vtt(t)),
        "txt" => Ok(to_txt(t)),
        "json" => serde_json::to_string_pretty(t).map_err(|e| e.to_string()),
        other => Err(format!("unsupported export format: {other}")),
    }
}

/// Format the last `transcribe` result as `"srt"`, `"vtt"`, `"txt"`, or `"json"`. Reads
/// whatever `TranscriptState` holds, so it needed no change to start formatting real ASR
/// output once `transcribe` went live.
#[tauri::command]
pub fn export_transcript(
    format: String,
    state: State<'_, TranscriptState>,
) -> Result<String, String> {
    let transcript = state
        .last_transcript
        .read()
        .map_err(|_| "transcript lock poisoned")?
        .clone()
        .ok_or_else(|| "transcribe the file first".to_string())?;
    render_export(&transcript, &format)
}

/// Write `content` to `path`, creating parent directories as needed — the Transcript tab's
/// "Save" action after `export_transcript` produces the text (no file-dialog plugin is
/// wired yet, so the destination is a text field, same pattern as the S2 Export tab).
#[tauri::command]
pub fn write_text_file(path: String, content: String) -> Result<(), String> {
    let target = PathBuf::from(&path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(&target, content).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_buffer(sr: u32, secs: f64) -> AudioBuffer {
        let n = (sr as f64 * secs) as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| 0.4 * ((i as f32 * 220.0 * std::f32::consts::TAU) / sr as f32).sin())
            .collect();
        AudioBuffer::from_planar(vec![samples.clone(), samples], sr)
    }

    // ---- wire <-> engine conversions ----

    #[test]
    fn word_from_asr_word_preserves_every_field() {
        let w = anvil_asr::Word {
            text: "hello".into(),
            start: 1.0,
            end: 1.4,
            confidence: 0.87,
            speaker: None,
        };
        let out = Word::from(w);
        assert_eq!(out.text, "hello");
        assert_eq!(out.start, 1.0);
        assert_eq!(out.end, 1.4);
        assert_eq!(out.confidence, 0.87);
    }

    #[test]
    fn transcript_from_asr_transcript_maps_words_and_segments() {
        let t = anvil_asr::Transcript {
            language: "en".into(),
            words: vec![anvil_asr::Word {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                confidence: 0.9,
                speaker: None,
            }],
            segments: vec![anvil_asr::Segment {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                speaker: None,
            }],
        };
        let out = Transcript::from(t);
        assert_eq!(out.language, "en");
        assert_eq!(out.words.len(), 1);
        assert_eq!(out.segments.len(), 1);
        assert_eq!(out.words[0].text, "hi");
    }

    #[test]
    fn cut_kind_round_trips_wire_strings() {
        assert_eq!(
            serde_json::to_string(&CutKind::from(anvil_cut::CutKind::Silence)).unwrap(),
            "\"silence\""
        );
        assert_eq!(
            serde_json::to_string(&CutKind::from(anvil_cut::CutKind::Filler)).unwrap(),
            "\"filler\""
        );
    }

    #[test]
    fn cut_plan_from_engine_plan_preserves_order_and_fields() {
        let engine_plan = anvil_cut::CutPlan {
            cuts: vec![anvil_cut::Cut {
                start: 1.0,
                end: 2.0,
                kind: anvil_cut::CutKind::Filler,
                label: "um".into(),
                accepted: false,
            }],
            source_duration: 10.0,
        };
        let wire = CutPlan::from(&engine_plan);
        assert_eq!(wire.cuts.len(), 1);
        assert_eq!(wire.cuts[0].start, 1.0);
        assert_eq!(wire.cuts[0].end, 2.0);
        assert_eq!(wire.cuts[0].kind, CutKind::Filler);
        assert_eq!(wire.cuts[0].label, "um");
        assert!(!wire.cuts[0].accepted);
    }

    // ---- model id resolution ----

    #[test]
    fn resolve_transcribe_model_rejects_unknown_ids_without_downloading() {
        let tmp = tempfile::tempdir().unwrap();
        // An empty models dir and no `anvil_asr` search-path env vars configured in the
        // test process, so nothing resolves — this proves the miss path returns `None`
        // (never panics, never fabricates a path).
        assert!(resolve_transcribe_model("does-not-exist", tmp.path()).is_none());
        assert!(resolve_transcribe_model("whisper-does-not-exist", tmp.path()).is_none());
    }

    #[test]
    fn resolve_transcribe_model_strips_the_models_screen_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        // "whisper-small" (a `models.rs` ModelPack id) should probe anvil_asr's "small"
        // catalog id, not fail immediately on the raw string.
        let direct = anvil_asr::locate_model("small");
        let via_prefix = resolve_transcribe_model("whisper-small", tmp.path());
        assert_eq!(direct, via_prefix);
    }

    #[test]
    fn resolve_transcribe_model_finds_the_models_screen_download() {
        let tmp = tempfile::tempdir().unwrap();
        // The Models screen's download dir has a real (fixture) `ggml-small.bin`. Whether it's
        // found via `anvil_asr::locate_model` (which now also searches the per-user config dir)
        // or the explicit `models_dir` fallback, resolution must land on a `ggml-small.bin`.
        // (Asserting the filename rather than the exact dir keeps this hermetic even on a
        // machine that already has a real whisper-small installed in the config dir.)
        std::fs::write(tmp.path().join("ggml-small.bin"), b"fixture bytes").unwrap();
        let found = resolve_transcribe_model("whisper-small", tmp.path());
        assert!(
            found
                .as_ref()
                .is_some_and(|p| p.ends_with("ggml-small.bin")),
            "whisper-small should resolve to a ggml-small.bin, got {found:?}"
        );
    }

    // ---- cut_options_for_mode ----

    #[test]
    fn cut_options_for_mode_selects_kinds() {
        let silence = cut_options_for_mode("silence");
        assert!(silence.cut_silence && !silence.cut_fillers);
        let filler = cut_options_for_mode("filler");
        assert!(!filler.cut_silence && filler.cut_fillers);
        let both = cut_options_for_mode("both");
        assert!(both.cut_silence && both.cut_fillers);
        let empty = cut_options_for_mode("");
        assert!(empty.cut_silence && empty.cut_fillers);
        let unknown = cut_options_for_mode("bogus");
        assert!(unknown.cut_silence && unknown.cut_fillers);
    }

    #[test]
    fn cut_options_for_mode_recognizes_aggressive() {
        let plain = cut_options_for_mode("both");
        assert!(!plain.aggressive);
        let aggressive = cut_options_for_mode("aggressive");
        assert!(aggressive.aggressive);
        assert!(aggressive.cut_silence && aggressive.cut_fillers);
        let filler_aggressive = cut_options_for_mode("filler-aggressive");
        assert!(filler_aggressive.aggressive);
        assert!(!filler_aggressive.cut_silence && filler_aggressive.cut_fillers);
    }

    // ---- staging WAV ----

    #[test]
    fn write_wav_16k_mono_downmixes_and_resamples() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("staged.wav");
        let buf = tone_buffer(48_000, 0.5);
        write_wav_16k_mono(&path, &buf).unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        // ~0.5 s at 16 kHz.
        let samples: Vec<i16> = reader.into_samples::<i16>().map(Result::unwrap).collect();
        assert!((samples.len() as i64 - 8_000).abs() < 50);
    }

    // ---- export formatting (unchanged by the M3 wiring) ----

    #[test]
    fn srt_and_vtt_round_trip_timestamps() {
        let t = Transcript {
            language: "en".into(),
            words: vec![],
            segments: vec![TranscriptSegment {
                text: "Hello world.".into(),
                start: 1.5,
                end: 3.25,
                speaker: None,
            }],
        };
        let srt = to_srt(&t);
        assert!(srt.contains("00:00:01,500 --> 00:00:03,250"));
        assert!(srt.contains("Hello world."));
        let vtt = to_vtt(&t);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:01.500 --> 00:00:03.250"));
    }

    #[test]
    fn txt_export_joins_segment_text() {
        let t = Transcript {
            language: "en".into(),
            words: vec![],
            segments: vec![
                TranscriptSegment {
                    text: "First.".into(),
                    start: 0.0,
                    end: 1.0,
                    speaker: None,
                },
                TranscriptSegment {
                    text: "Second.".into(),
                    start: 1.0,
                    end: 2.0,
                    speaker: None,
                },
            ],
        };
        assert_eq!(to_txt(&t), "First.\n\nSecond.");
    }

    #[test]
    fn json_export_round_trips_through_serde() {
        let t = Transcript {
            language: "en".into(),
            words: vec![Word {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                confidence: 0.9,
                speaker: None,
            }],
            segments: vec![TranscriptSegment {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                speaker: None,
            }],
        };
        let json = render_export(&t, "JSON").unwrap();
        let back: Transcript = serde_json::from_str(&json).unwrap();
        assert_eq!(back.words.len(), t.words.len());
    }

    #[test]
    fn unsupported_export_format_is_a_clear_error() {
        let t = Transcript::default();
        let err = render_export(&t, "docx").unwrap_err();
        assert!(err.contains("docx"));
    }

    // ---- diarization mapping (no sherpa sidecar needed) ----

    #[test]
    fn to_asr_transcript_round_trips_speaker_fields() {
        let t = Transcript {
            language: "en".into(),
            words: vec![Word {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                confidence: 0.9,
                speaker: Some(1),
            }],
            segments: vec![TranscriptSegment {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                speaker: Some(1),
            }],
        };
        let back = Transcript::from(to_asr_transcript(&t));
        assert_eq!(back.words[0].speaker, Some(1));
        assert_eq!(back.segments[0].speaker, Some(1));
    }

    #[test]
    fn assign_speakers_stamps_the_wire_transcript_through_the_conversion() {
        // The exact path `diarize` uses: wire -> asr -> assign_speakers -> wire, but with a
        // hand-built Diarization so it needs no sherpa binary.
        let t = Transcript {
            language: "en".into(),
            words: vec![Word {
                text: "hello".into(),
                start: 0.2,
                end: 0.7,
                confidence: 0.9,
                speaker: None,
            }],
            segments: vec![TranscriptSegment {
                text: "hello".into(),
                start: 0.0,
                end: 1.0,
                speaker: None,
            }],
        };
        let mut asr = to_asr_transcript(&t);
        let diar = anvil_asr::Diarization {
            speakers: vec![anvil_asr::Speaker {
                id: 0,
                label: "Speaker 1".into(),
            }],
            segments: vec![anvil_asr::SpeakerSegment {
                speaker: 0,
                start: 0.0,
                end: 5.0,
            }],
        };
        anvil_asr::assign_speakers(&mut asr, &diar);
        let wire = Transcript::from(asr);
        assert_eq!(wire.words[0].speaker, Some(0));
        assert_eq!(wire.segments[0].speaker, Some(0));
    }

    // ---- end-to-end against the real whisper.cpp sidecar (gated on the test assets) ----

    /// Exercises the exact staging + transcribe path `transcribe` uses, against the real
    /// `whisper-cli` sidecar and a real ggml model — requires `ANVIL_WHISPER` +
    /// `ANVIL_WHISPER_MODEL` to point at them (see the M3 UI-wiring task's test assets).
    #[test]
    fn transcribe_pipeline_runs_against_the_real_whisper_sidecar() {
        if std::env::var_os("ANVIL_WHISPER").is_none() {
            eprintln!("skipping: ANVIL_WHISPER not set");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("staged.wav");
        // A few seconds of a pure tone: whisper won't recognize words in it, but this
        // proves the sidecar runs end-to-end (spawns, reads the model, returns valid JSON)
        // without requiring a spoken-word fixture in the repo.
        let buf = tone_buffer(48_000, 3.0);
        write_wav_16k_mono(&staged, &buf).unwrap();

        let opts = anvil_asr::TranscribeOptions::default();
        let transcript = anvil_asr::transcribe(&staged, &opts)
            .expect("real whisper-cli sidecar should transcribe the staged WAV");
        // No assertion on word content (a tone has none worth recognizing) — reaching here
        // proves the sidecar launched, read the real model, and returned parseable JSON.
        assert!(
            transcript.words.len() < 50,
            "unexpectedly many words from a tone"
        );
    }
}
