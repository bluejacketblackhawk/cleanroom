//! Split screen commands (handoff/09 §1): detect every spoken cut word in the open file,
//! preview the resulting segments live as matches are accepted/rejected, and render each
//! segment as its own (optionally mastered) file — the cut word and the dead air around it
//! deleted entirely.
//!
//! Detection runs the real `anvil_cut::detect_cut_words` engine over the cached Transcript
//! tab result (transcribing first, via the exact staging path `transcript::transcribe`
//! uses, when nothing is cached). Rendering is the CLI's `split` flow re-hosted:
//! `anvil_cut::apply_with_edge_fades` per part, `anvil_dsp::master_buffer` per part (09 §4
//! — every short stands alone), 16-bit WAV for audio sources, and per-segment H.264
//! re-encode via `anvil_media::export_video_segment` for video sources. Naming helpers are
//! duplicated from `anvil-cli` (this crate deliberately doesn't depend on the CLI — same
//! precedent as `transcript::write_wav_16k_mono`).

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use anvil_cut::{SilenceInput, SplitOptions};
use anvil_media::AudioBuffer;
use anvil_project::{preset, Settings};

use crate::AudioState;

/// One detected cut-word instance — an accept/reject row in the Split screen's review list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitMatch {
    pub start: f64,
    pub end: f64,
    /// What the ASR actually heard (a fuzzy row shows e.g. `"Comquad"`).
    pub label: String,
    pub accepted: bool,
}

/// One preview segment: where it spans and what it will be called.
#[derive(Debug, Clone, Serialize)]
pub struct SplitPart {
    pub index: usize,
    pub start: f64,
    pub end: f64,
    /// The part's first transcribed words — drives the `{first_words}` naming token.
    pub title: String,
    pub duration: f64,
}

/// `split_detect`/`split_preview` result: the review rows plus the live segment preview.
#[derive(Debug, Clone, Serialize)]
pub struct SplitPreview {
    pub matches: Vec<SplitMatch>,
    pub parts: Vec<SplitPart>,
    /// Video sources re-encode per segment (H.264) and keep their container.
    pub is_video: bool,
}

/// One rendered segment in the `split_render` result.
#[derive(Debug, Clone, Serialize)]
pub struct SplitSegmentResult {
    pub index: usize,
    pub title: String,
    pub path: String,
    pub duration: f64,
    pub lufs_in: Option<f64>,
    pub lufs_out: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitRenderResult {
    pub out_dir: String,
    pub segments: Vec<SplitSegmentResult>,
}

/// Coarse render progress for the Split screen (`split://progress`).
#[derive(Debug, Clone, Serialize)]
pub struct SplitProgressEvent {
    pub index: usize,
    pub total: usize,
    pub fraction: f32,
}

/// What `split_detect` computed, held so accept/reject toggles and the render don't redo
/// analysis/ASR. Reset implicitly per file: `split_detect` always recomputes from the
/// currently open buffer.
#[derive(Default)]
pub struct SplitState {
    computed: RwLock<Option<Computed>>,
}

struct Computed {
    cuts: Vec<anvil_cut::Cut>,
    words: Vec<anvil_cut::Word>,
    silence: SilenceInput,
    duration: f64,
    opts: SplitOptions,
}

impl SplitState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The open file's source path, or a friendly error.
fn source_path(state: &AudioState) -> Result<PathBuf, String> {
    state
        .source_path
        .read()
        .map_err(|_| "path lock poisoned")?
        .clone()
        .ok_or_else(|| "open a file before splitting".to_string())
}

fn wire_matches(cuts: &[anvil_cut::Cut]) -> Vec<SplitMatch> {
    cuts.iter()
        .map(|c| SplitMatch {
            start: c.start,
            end: c.end,
            label: c.label.clone(),
            accepted: c.accepted,
        })
        .collect()
}

fn wire_parts(plan: &anvil_cut::SegmentPlan) -> Vec<SplitPart> {
    plan.parts
        .iter()
        .map(|p| SplitPart {
            index: p.index,
            start: p.start,
            end: p.end,
            title: p.title(),
            duration: p.kept_secs(),
        })
        .collect()
}

fn preview_from(computed: &Computed, is_video: bool) -> SplitPreview {
    let plan = anvil_cut::CutPlan {
        cuts: computed.cuts.clone(),
        source_duration: computed.duration,
    };
    let segment_plan =
        anvil_cut::partition(&plan, &computed.words, &computed.silence, &computed.opts);
    SplitPreview {
        matches: wire_matches(&computed.cuts),
        parts: wire_parts(&segment_plan),
        is_video,
    }
}

/// Detect every instance of `cut_word` in the currently open file (09 §2). Reuses the
/// Transcript tab's cached transcript when one exists; otherwise transcribes with `model`
/// first (and caches the result for the Transcript tab — one transcription serves both).
#[tauri::command]
pub fn split_detect(
    cut_word: String,
    model: String,
    state: State<'_, AudioState>,
    tstate: State<'_, crate::transcript::TranscriptState>,
    mstate: State<'_, crate::models::ModelsState>,
    sstate: State<'_, SplitState>,
) -> Result<SplitPreview, String> {
    let phrase = cut_word.trim().to_string();
    if phrase.is_empty() {
        return Err("pick a cut word first (you can save a default in Settings)".into());
    }

    let buffer = {
        let guard = state.original.read().map_err(|_| "audio lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before splitting".to_string())?
            .clone()
    };
    let is_video = anvil_media::is_video_container(&source_path(&state)?);
    let duration = buffer.frames() as f64 / f64::from(buffer.sample_rate().max(1));

    // Transcript: cached, or transcribe now via the Transcript tab's exact staging path.
    let transcript = match tstate.snapshot() {
        Some(t) => t,
        None => {
            let model_path = crate::transcript::resolve_transcribe_model(&model, mstate.dir())
                .ok_or_else(|| {
                    format!(
                        "the \"{model}\" model isn't installed — install it in Models, then try \
                         again."
                    )
                })?;
            let staged = crate::transcript::unique_temp_wav_path();
            crate::transcript::write_wav_16k_mono(&staged, &buffer)
                .map_err(|e| format!("could not stage audio for transcription: {e}"))?;
            let opts = anvil_asr::TranscribeOptions {
                model: Some(model_path),
                ..anvil_asr::TranscribeOptions::default()
            };
            let result = anvil_asr::transcribe(&staged, &opts);
            let _ = std::fs::remove_file(&staged);
            let asr = result.map_err(|e| format!("transcribe failed: {e}"))?;
            let wire = crate::transcript::Transcript::from(asr);
            tstate.store_transcript(wire.clone())?;
            wire
        }
    };

    let words: Vec<anvil_cut::Word> = transcript
        .words
        .iter()
        .map(|w| anvil_cut::Word {
            text: w.text.clone(),
            start: w.start,
            end: w.end,
            confidence: w.confidence,
        })
        .collect();

    let analysis = anvil_dsp::analyze_buffer(&buffer);
    let silence = SilenceInput::from_runs(analysis.silence_runs.iter().map(|r| (r.start, r.end)));

    let opts = SplitOptions::for_phrase(phrase);
    let cuts = anvil_cut::detect_cut_words(&words, &silence, &opts);

    let computed = Computed {
        cuts,
        words,
        silence,
        duration,
        opts,
    };
    let preview = preview_from(&computed, is_video);
    *sstate.computed.write().map_err(|_| "split lock poisoned")? = Some(computed);
    Ok(preview)
}

/// Re-preview with a new complete set of accepted match indices (the review list's
/// accept/reject toggles) — pure recomputation, no ASR, no analysis.
#[tauri::command]
pub fn split_preview(
    accepted_indices: Vec<usize>,
    state: State<'_, AudioState>,
    sstate: State<'_, SplitState>,
) -> Result<SplitPreview, String> {
    let accepted: std::collections::HashSet<usize> = accepted_indices.into_iter().collect();
    let mut guard = sstate.computed.write().map_err(|_| "split lock poisoned")?;
    let computed = guard
        .as_mut()
        .ok_or_else(|| "detect cut words before previewing".to_string())?;
    for (i, cut) in computed.cuts.iter_mut().enumerate() {
        cut.accepted = accepted.contains(&i);
    }
    let is_video = anvil_media::is_video_container(&source_path(&state)?);
    Ok(preview_from(computed, is_video))
}

// ---- naming helpers (duplicated from anvil-cli's split verb — see module docs) -----------

fn sanitize_segment_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string();
    if trimmed.is_empty() {
        "part".into()
    } else {
        trimmed
    }
}

fn render_segment_name(
    template: &str,
    n: usize,
    total: usize,
    source_stem: &str,
    first_words: &str,
) -> String {
    let width = if total >= 100 { 3 } else { 2 };
    let rendered = template
        .replace("{nn}", &format!("{n:0width$}"))
        .replace("{n}", &n.to_string())
        .replace("{name}", source_stem)
        .replace("{first_words}", first_words);
    sanitize_segment_name(&rendered)
}

fn unique_segment_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut k = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({k}).{ext}"));
        k += 1;
    }
    path
}

fn write_wav_16(path: &Path, audio: &AudioBuffer) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: audio.channel_count().max(1) as u16,
        sample_rate: audio.sample_rate(),
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    let channels = audio.channel_count();
    for f in 0..audio.frames() {
        for c in 0..channels {
            let s = audio.channel(c)[f];
            writer
                .write_sample((s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
                .map_err(|e| e.to_string())?;
        }
    }
    writer.finalize().map_err(|e| e.to_string())
}

/// Render the accepted split (09 §4–5): every part gets its own edge-faded render,
/// optionally its own full master, and its own file — `{nn} {first_words}` names in
/// `<stem>_segments/` beside the source unless overridden. Video sources re-encode
/// (H.264, container kept; webm lands as mp4); audio sources write 16-bit WAV. Coarse
/// progress streams on `split://progress`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn split_render(
    accepted_indices: Vec<usize>,
    out_dir: Option<String>,
    master: bool,
    preset_ref: Option<String>,
    app: AppHandle,
    state: State<'_, AudioState>,
    pstate: State<'_, crate::presets::PresetsState>,
    sstate: State<'_, SplitState>,
) -> Result<SplitRenderResult, String> {
    let accepted: std::collections::HashSet<usize> = accepted_indices.into_iter().collect();
    let (plan, words, silence, opts) = {
        let mut guard = sstate.computed.write().map_err(|_| "split lock poisoned")?;
        let computed = guard
            .as_mut()
            .ok_or_else(|| "detect cut words before splitting".to_string())?;
        for (i, cut) in computed.cuts.iter_mut().enumerate() {
            cut.accepted = accepted.contains(&i);
        }
        (
            anvil_cut::CutPlan {
                cuts: computed.cuts.clone(),
                source_duration: computed.duration,
            },
            computed.words.clone(),
            computed.silence.clone(),
            computed.opts.clone(),
        )
    };

    let buffer = {
        let guard = state.original.read().map_err(|_| "audio lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before splitting".to_string())?
            .clone()
    };
    let source = source_path(&state)?;
    let is_video = anvil_media::is_video_container(&source);

    let segment_plan = anvil_cut::partition(&plan, &words, &silence, &opts);
    let total = segment_plan.parts.len();
    if total == 0 {
        return Err("no segments to render — accept at least one cut".into());
    }

    // Per-segment master target: shorts platforms for video, the podcast default for audio
    // (09 §1), unless the caller passed a preset.
    let preset_id = preset_ref.unwrap_or_else(|| {
        if is_video {
            preset::SPOTIFY_YOUTUBE_ID.to_string()
        } else {
            preset::PODCAST_STEREO_ID.to_string()
        }
    });
    let resolved_preset = crate::presets::resolve_preset_ref(&preset_id, pstate.dir())?;

    let settings = Settings::load(&Settings::default_path()).unwrap_or_default();
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recording")
        .to_string();
    let dir = out_dir.map(PathBuf::from).unwrap_or_else(|| {
        source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{stem}_segments"))
    });
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let ext = if is_video {
        match source
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("webm") | None => "mp4".to_string(),
            Some(e) => e.to_string(),
        }
    } else {
        "wav".to_string()
    };

    let sidecar = if is_video {
        Some(anvil_media::FfmpegSidecar::locate().map_err(|e| e.to_string())?)
    } else {
        None
    };

    let mut segments = Vec::with_capacity(total);
    for part in &segment_plan.parts {
        let n = part.index + 1;
        let emit = |fraction: f32| {
            let _ = app.emit(
                "split://progress",
                SplitProgressEvent {
                    index: part.index,
                    total,
                    fraction: (part.index as f32 + fraction) / total as f32,
                },
            );
        };
        emit(0.05);

        let name = render_segment_name(
            &settings.split_name_template,
            n,
            total,
            &stem,
            &part.title(),
        );
        let out_path = unique_segment_path(&dir, &name, &ext);

        let part_audio = anvil_cut::apply_with_edge_fades(
            &part.edl,
            &buffer,
            anvil_cut::DEFAULT_CROSSFADE_SECS,
            anvil_cut::DEFAULT_EDGE_FADE_SECS,
        );

        let (final_audio, lufs) = if master {
            let r = anvil_dsp::master_buffer(&part_audio, &resolved_preset, resolved_preset.tier)
                .map_err(|e| format!("mastering segment {n} failed: {e}"))?;
            let lufs = (
                r.report.before.integrated_lufs,
                r.report.after.integrated_lufs,
            );
            (r.audio, Some(lufs))
        } else {
            (part_audio, None)
        };
        emit(0.5);

        if let Some(sidecar) = sidecar.as_ref() {
            anvil_media::export_video_segment(
                sidecar,
                &source,
                part.start,
                part.end - part.start,
                &final_audio,
                &out_path,
                |f| emit(0.5 + 0.5 * f),
            )
            .map_err(|e| format!("segment {n} ({}): {e}", out_path.display()))?;
        } else {
            write_wav_16(&out_path, &final_audio)
                .map_err(|e| format!("segment {n} ({}): {e}", out_path.display()))?;
        }
        emit(1.0);

        segments.push(SplitSegmentResult {
            index: part.index,
            title: part.title(),
            path: out_path.display().to_string(),
            duration: part.kept_secs(),
            lufs_in: lufs.map(|l| l.0),
            lufs_out: lufs.map(|l| l.1),
        });
    }

    Ok(SplitRenderResult {
        out_dir: dir.display().to_string(),
        segments,
    })
}

// ---- app settings (the "picking the cutword … in settings" half of 09 §1) ---------------

/// The Settings screen's slice of `anvil_project::Settings` relevant to splitting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettingsWire {
    pub default_cut_word: Option<String>,
    pub split_name_template: String,
    pub split_master_default: bool,
}

#[tauri::command]
pub fn app_settings_get() -> Result<AppSettingsWire, String> {
    let s = Settings::load(&Settings::default_path()).map_err(|e| e.to_string())?;
    Ok(AppSettingsWire {
        default_cut_word: s.default_cut_word,
        split_name_template: s.split_name_template,
        split_master_default: s.split_master_default,
    })
}

#[tauri::command]
pub fn app_settings_set(patch: AppSettingsWire) -> Result<(), String> {
    let path = Settings::default_path();
    let mut s = Settings::load(&path).map_err(|e| e.to_string())?;
    s.default_cut_word = patch
        .default_cut_word
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty());
    s.split_name_template = if patch.split_name_template.trim().is_empty() {
        "{nn} {first_words}".to_string()
    } else {
        patch.split_name_template
    };
    s.split_master_default = patch.split_master_default;
    s.save(&path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_render_and_sanitize() {
        assert_eq!(
            render_segment_name("{nn} {first_words}", 2, 3, "take", "why: nobody/holds"),
            "02 why nobody holds"
        );
        assert_eq!(
            render_segment_name("{first_words}", 1, 2, "take", ""),
            "part"
        );
    }

    #[test]
    fn unique_segment_path_suffixes_instead_of_overwriting() {
        let tmp = tempfile::tempdir().unwrap();
        let a = unique_segment_path(tmp.path(), "01 intro", "wav");
        std::fs::write(&a, b"x").unwrap();
        let b = unique_segment_path(tmp.path(), "01 intro", "wav");
        assert_eq!(b, tmp.path().join("01 intro (2).wav"));
    }

    #[test]
    fn settings_wire_round_trips_through_json() {
        let w = AppSettingsWire {
            default_cut_word: Some("kumquat".into()),
            split_name_template: "{nn} {first_words}".into(),
            split_master_default: true,
        };
        let json = serde_json::to_string(&w).unwrap();
        let back: AppSettingsWire = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_cut_word.as_deref(), Some("kumquat"));
        assert!(back.split_master_default);
    }
}
