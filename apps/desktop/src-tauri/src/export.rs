//! Export commands (04 §S2 Export tab): real encoders via `anvil_media::encode` (WAV stays
//! on the `hound` PCM writer — there's no encoder to shell out to for uncompressed audio),
//! simultaneous outputs from one mastered buffer, per-output progress events, and an
//! optional compliance report (`anvil_project::compliance`) built from the last `master`
//! run's real measurements.

use std::path::{Path, PathBuf};

use anvil_media::{AudioBuffer, OutputFormat};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::AudioState;

/// One requested export output (04 §S2 Export tab): format + bitrate (encoded formats
/// only) + mono/stereo + destination path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    /// "wav" | "mp3" | "flac" | "opus" | "aac".
    pub format: String,
    pub path: String,
    #[serde(default)]
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub mono: Option<bool>,
}

/// Per-output outcome of an export run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputResult {
    pub path: String,
    pub ok: bool,
    pub message: Option<String>,
}

/// Result of `export_outputs`: overall success + one [`OutputResult`] per requested
/// output, in the same order, plus the compliance report's path if one was requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    pub ok: bool,
    pub outputs: Vec<OutputResult>,
    /// Path to the written HTML report, if `compliance` was requested and it succeeded.
    pub compliance_report: Option<String>,
    /// Why the compliance report wasn't written, if it was requested and failed. Export
    /// itself can still be `ok`even when this is set — the audio outputs are independent
    /// of the report.
    pub compliance_error: Option<String>,
}

/// Emitted on `export://progress` as each output's ffmpeg sidecar reports completion
/// fraction, so the S2 Export tab can show a per-row progress bar.
#[derive(Debug, Clone, Serialize)]
struct ExportProgressEvent {
    index: usize,
    fraction: f32,
}

/// Export the mastered buffer (falling back to the original if Master hasn't run yet) to
/// every requested output, from the one in-memory buffer (no re-decode/re-render between
/// outputs — 04 §S2 "simultaneous outputs"). When `compliance` is set, also writes an HTML
/// + PDF report next to the first successful output.
#[tauri::command]
pub fn export_outputs(
    outputs: Vec<OutputSpec>,
    compliance: bool,
    app: AppHandle,
    state: State<'_, AudioState>,
) -> ExportResult {
    let buffer = {
        let processed = state.processed.read().ok().and_then(|g| g.clone());
        processed.or_else(|| state.original.read().ok().and_then(|g| g.clone()))
    };

    let results: Vec<OutputResult> = outputs
        .into_iter()
        .enumerate()
        .map(|(index, spec)| {
            let Some(ref audio) = buffer else {
                return OutputResult {
                    path: spec.path,
                    ok: false,
                    message: Some("open and master a file first".to_string()),
                };
            };
            let target = PathBuf::from(&spec.path);
            let app = app.clone();
            let result = encode_one(audio, &spec, &target, move |fraction| {
                let _ = app.emit("export://progress", ExportProgressEvent { index, fraction });
            });
            match result {
                Ok(()) => OutputResult {
                    path: target.to_string_lossy().into_owned(),
                    ok: true,
                    message: None,
                },
                Err(e) => OutputResult {
                    path: target.to_string_lossy().into_owned(),
                    ok: false,
                    message: Some(e),
                },
            }
        })
        .collect();
    let ok = results.iter().all(|r| r.ok);

    let (compliance_report, compliance_error) = if compliance {
        match write_compliance_report(&state, &results) {
            Ok(path) => (Some(path), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };

    ExportResult {
        ok,
        outputs: results,
        compliance_report,
        compliance_error,
    }
}

/// Encode one output. WAV is written directly (16-bit PCM via `hound`, with a real
/// channel-mixing downmix when mono is requested — matching an ffmpeg output-side `-ac 1`
/// mix rather than corrupting the stream). Every other format goes through
/// `anvil_media::encode`'s ffmpeg sidecar.
///
/// Renders to a sibling temp file and only renames onto `target` once the encode finishes
/// without error (05 §M5.F crash recovery). If the app is killed mid-render — a crash, a
/// forced shutdown, power loss — the user is left with either the previous file at `target`
/// untouched, or nothing there at all; never a truncated/corrupt file that looks like a
/// finished export. The temp file keeps `target`'s real extension (just with an
/// `.anviltmp-<pid>-<nanos>` infix before it) so format auto-detection by extension still
/// works during the render.
fn encode_one(
    audio: &AudioBuffer,
    spec: &OutputSpec,
    target: &Path,
    progress: impl FnMut(f32),
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let tmp_target = temp_sibling(target);
    let result = encode_to(audio, spec, &tmp_target, progress);
    match result {
        Ok(()) => std::fs::rename(&tmp_target, target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_target);
            format!(
                "wrote output but could not finalize it at {}: {e}",
                target.display()
            )
        }),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_target);
            Err(e)
        }
    }
}

fn encode_to(
    audio: &AudioBuffer,
    spec: &OutputSpec,
    target: &Path,
    progress: impl FnMut(f32),
) -> Result<(), String> {
    match spec.format.to_ascii_lowercase().as_str() {
        "wav" => {
            let mut progress = progress;
            let mixed = downmix_for_wav(audio, spec.mono.unwrap_or(false));
            let result = write_wav_16bit(target, &mixed);
            progress(1.0);
            result
        }
        "mp3" => encode_with(audio, spec, OutputFormat::Mp3, target, progress),
        "flac" => encode_with(audio, spec, OutputFormat::Flac, target, progress),
        "opus" => encode_with(audio, spec, OutputFormat::Opus, target, progress),
        "aac" => encode_with(audio, spec, OutputFormat::Aac, target, progress),
        other => Err(format!("unsupported export format: {other}")),
    }
}

/// A same-directory temp path for `target` that keeps its real extension at the end (so
/// ffmpeg's extension-based container/codec auto-detection still works) but can never
/// collide with a concurrent export or a previous crash's leftovers. `pub(crate)` so
/// `clip_studio::render` can reuse it for the same crash-recovery guarantee on its MP4
/// output.
pub(crate) fn temp_sibling(target: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let infix = format!(".anviltmp-{}-{nanos}", std::process::id());
    match target.extension().and_then(|e| e.to_str()) {
        Some(ext) => target.with_file_name(format!("{stem}{infix}.{ext}")),
        None => target.with_file_name(format!("{stem}{infix}")),
    }
}

fn encode_with(
    audio: &AudioBuffer,
    spec: &OutputSpec,
    format: OutputFormat,
    target: &Path,
    progress: impl FnMut(f32),
) -> Result<(), String> {
    let mut out = anvil_media::OutputSpec::new(format);
    if let Some(kbps) = spec.bitrate {
        out = out.with_bitrate(kbps);
    }
    out = out.with_mono(spec.mono.unwrap_or(false));
    anvil_media::encode_with_progress(audio, &out, target, progress).map_err(|e| e.to_string())
}

/// Mix down to mono for the WAV path (the encode-module downmix is an ffmpeg output flag;
/// the raw-PCM WAV writer has no such flag, so this does the equal-weight channel average
/// itself). A no-op (clone) when `mono` is false or the source is already mono.
fn downmix_for_wav(audio: &AudioBuffer, mono: bool) -> AudioBuffer {
    let channels = audio.channel_count();
    if !mono || channels <= 1 {
        return audio.clone();
    }
    let frames = audio.frames();
    let mut mixed = vec![0.0f32; frames];
    for ch in 0..channels {
        for (i, s) in audio.channel(ch).iter().enumerate() {
            mixed[i] += s / channels as f32;
        }
    }
    AudioBuffer::from_planar(vec![mixed], audio.sample_rate())
}

/// Write `audio` to a fresh temp WAV named `<prefix>-<pid>-<nanos>.wav`, returning its
/// path. Shared by callers that need a real file on disk to hand to `open_media` (or
/// ffmpeg) without duplicating the WAV-writing logic: `multitrack::multitrack_mix` (the
/// mixdown becomes an openable file the existing Master/Export commands work on unchanged)
/// and `clip_studio`'s audio staging.
pub(crate) fn write_temp_wav(audio: &AudioBuffer, prefix: &str) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}.wav", std::process::id()));
    write_wav_16bit(&path, audio)?;
    Ok(path)
}

/// Write a planar-f32 buffer to a 16-bit PCM WAV (dither is applied in-chain for 16-bit).
fn write_wav_16bit(path: &Path, audio: &AudioBuffer) -> Result<(), String> {
    let channels = audio.channel_count().max(1);
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: audio.sample_rate(),
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for frame in 0..audio.frames() {
        for ch in 0..channels {
            let s = audio.channel(ch).get(frame).copied().unwrap_or(0.0);
            let q = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            writer.write_sample(q).map_err(|e| e.to_string())?;
        }
    }
    writer.finalize().map_err(|e| e.to_string())
}

/// Whole-program RMS in dBFS across every channel/sample of `audio`. ACX itself grades on
/// a windowed RMS; this is the simpler whole-file figure — real and directly measured
/// (not a placeholder), just not windowed. Good enough for the report's ACX row; a
/// windowed measure is a fair follow-up if the report's ACX pass/fail proves too coarse in
/// practice.
fn rms_dbfs(audio: &AudioBuffer) -> f64 {
    let mut sum_sq = 0.0f64;
    let mut count = 0usize;
    for ch in 0..audio.channel_count() {
        for &s in audio.channel(ch) {
            sum_sq += f64::from(s) * f64::from(s);
            count += 1;
        }
    }
    if count == 0 {
        return f64::NEG_INFINITY;
    }
    let rms = (sum_sq / count as f64).sqrt();
    if rms <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * rms.log10()
    }
}

/// Build a [`anvil_project::ComplianceInput`] from the last `master` run's stored
/// measurements + the buffer just exported, and write the HTML+PDF report next to the
/// first successfully exported output.
fn write_compliance_report(state: &AudioState, results: &[OutputResult]) -> Result<String, String> {
    let report = state
        .last_report
        .read()
        .map_err(|_| "report lock poisoned")?
        .clone()
        .ok_or_else(|| "master the file first — nothing to report on".to_string())?;
    let preset = state
        .last_preset
        .read()
        .map_err(|_| "preset lock poisoned")?
        .clone()
        .ok_or_else(|| "master the file first — nothing to report on".to_string())?;
    let preset_id = state
        .last_preset_ref
        .read()
        .map_err(|_| "preset lock poisoned")?
        .clone();
    let source_path = state
        .source_path
        .read()
        .map_err(|_| "path lock poisoned")?
        .clone()
        .ok_or_else(|| "no source file".to_string())?;
    let buffer = state
        .processed
        .read()
        .map_err(|_| "audio lock poisoned")?
        .clone()
        .ok_or_else(|| "master the file first".to_string())?;

    let output_file = results
        .iter()
        .find(|r| r.ok)
        .map(|r| r.path.clone())
        .unwrap_or_else(|| source_path.to_string_lossy().into_owned());

    let after_analysis = anvil_dsp::analyze_buffer(&buffer);

    let input = anvil_project::ComplianceInput {
        source_file: source_path.to_string_lossy().into_owned(),
        output_file: output_file.clone(),
        preset,
        preset_id,
        duration_secs: buffer.frames() as f64 / f64::from(buffer.sample_rate().max(1)),
        sample_rate: buffer.sample_rate(),
        channels: buffer.channel_count() as u32,
        before: anvil_project::LoudnessMeasurement {
            integrated_lufs: report.before.integrated_lufs,
            true_peak_dbtp: report.before.true_peak_dbtp,
            loudness_range_lu: report.before.loudness_range_lu,
        },
        after: anvil_project::LoudnessMeasurement {
            integrated_lufs: report.after.integrated_lufs,
            true_peak_dbtp: report.after.true_peak_dbtp,
            loudness_range_lu: report.after.loudness_range_lu,
        },
        rms_dbfs_out: Some(rms_dbfs(&buffer)),
        noise_floor_dbfs_out: Some(after_analysis.noise_floor_dbfs),
        modules: report
            .modules
            .iter()
            .map(|m| {
                if m.engaged {
                    anvil_project::ModuleDecision::applied(m.name.clone(), m.detail.clone())
                } else {
                    anvil_project::ModuleDecision::bypassed(m.name.clone(), m.detail.clone())
                }
            })
            .collect(),
        chain_version: report.chain_version,
    };

    // Matches `anvil_project::compliance::write_reports`'s own doc example
    // ("episode.wav.report.html") — append rather than replace the output's extension, so
    // the report sits next to a file whose name is still recognizable.
    let html_path = PathBuf::from(format!("{output_file}.report.html"));
    let pdf_path = PathBuf::from(format!("{output_file}.report.pdf"));
    anvil_project::compliance::write_reports(&input, &html_path, &pdf_path)
        .map_err(|e| e.to_string())?;
    Ok(html_path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_for_wav_averages_channels() {
        let buf = AudioBuffer::from_planar(vec![vec![1.0, 0.5], vec![-1.0, 0.5]], 48_000);
        let mono = downmix_for_wav(&buf, true);
        assert_eq!(mono.channel_count(), 1);
        assert!((mono.channel(0)[0] - 0.0).abs() < 1e-6);
        assert!((mono.channel(0)[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn downmix_for_wav_is_noop_when_not_requested() {
        let buf = AudioBuffer::from_planar(vec![vec![1.0], vec![-1.0]], 48_000);
        let stereo = downmix_for_wav(&buf, false);
        assert_eq!(stereo.channel_count(), 2);
    }

    #[test]
    fn rms_dbfs_of_full_scale_square_wave_is_near_zero() {
        // A signal that alternates +/-1.0 has RMS 1.0 -> 0 dBFS.
        let buf = AudioBuffer::from_planar(vec![vec![1.0, -1.0, 1.0, -1.0]], 48_000);
        let db = rms_dbfs(&buf);
        assert!(db.abs() < 1e-6, "expected ~0 dBFS, got {db}");
    }

    #[test]
    fn rms_dbfs_of_silence_is_negative_infinity() {
        let buf = AudioBuffer::silence(1, 100, 48_000);
        assert_eq!(rms_dbfs(&buf), f64::NEG_INFINITY);
    }

    #[test]
    fn unsupported_format_is_a_clear_error() {
        let buf = AudioBuffer::silence(1, 10, 48_000);
        let spec = OutputSpec {
            format: "ogg".into(),
            path: "x.ogg".into(),
            bitrate: None,
            mono: None,
        };
        let err = encode_one(&buf, &spec, Path::new("x.ogg"), |_| {}).unwrap_err();
        assert!(err.contains("ogg"));
    }
}
