//! `anvil` — the headless CLI (ADR-007). JSON-first output, stable exit codes; this is
//! both the automation surface and the QA harness's engine (04 §CLI acceptance: "every
//! S2/S4 capability scriptable headless").
//!
//! M2 fills in `master`'s real encoders (`anvil_media::encode`), the shipped-preset
//! registry (`anvil_project::Preset::by_id`), `batch`/`batch --watch`
//! (`anvil_batch::{BatchQueue, WatchService}`), `models list`/`pull`, and the
//! `--report` compliance document (`anvil_project::compliance`). M3 adds `transcribe` —
//! whisper.cpp word timestamps via `anvil_asr`, emitted as SRT/VTT/TXT/JSON.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use anvil_batch::{BatchItemState, BatchQueue, OutputSettings, WatchRule, WatchService};
use anvil_dsp::BlockSink;
use anvil_media::{AudioBuffer, OutputFormat, OutputSpec, StreamEncoder};
use anvil_project::{
    compliance::{ComplianceInput, LoudnessMeasurement, ModuleDecision},
    preset, Preset, Tier,
};

// ---- Stable exit codes (handoff/02-ARCHITECTURE.md §CLI, ADR-007) -------------------------
const EXIT_OK: u8 = 0;
const EXIT_BAD_INPUT: u8 = 2;
/// Preset id or model pack didn't resolve.
const EXIT_MISSING: u8 = 3;
/// Reserved for a cooperatively-cancelled run (e.g. a future Ctrl-C handler on
/// `batch --watch`). Nothing in this build triggers it yet — every command here runs
/// to completion synchronously — but the code is part of the stable contract so it's
/// defined now rather than invented later.
#[allow(dead_code)]
const EXIT_CANCELLED: u8 = 4;
const EXIT_INTERNAL: u8 = 5;

/// Shipped presets, formatted for `--help` (04 §CLI: "list the ids"). Hand-written
/// rather than built from `Preset::shipped()` at const-eval time — clap needs a
/// `'static str` for `after_help`, and the seven ids/names are a stable contract
/// (`anvil_project::preset` module docs: "never rename one once shipped") so this
/// can't drift silently.
const PRESET_HELP: &str = "\
Shipped presets (anvil_project::Preset::by_id):
  podcast_stereo       Podcast (Stereo -16 LUFS)      [default]
  podcast_mono         Podcast (Mono -19 LUFS)
  spotify_youtube      Spotify/YouTube (-14 LUFS)
  broadcast_ebu        Broadcast EBU R128 (-23 LUFS)
  audiobook_acx        Audiobook (ACX submission checklist)
  voice_memo_cleanup   Voice memo cleanup (fast tier)
  music_heavy_show     Music-heavy show (fast tier, lighter denoise)

Friendly aliases also accepted (case/hyphen/underscore-insensitive): stereo, podcast,
mono, spotify, youtube, streaming, broadcast, ebu, r128, acx, audiobook, voice, voice-memo,
music, music-heavy.";

#[derive(Parser)]
#[command(
    name = "anvil",
    version,
    about = "Cleanroom — local podcast mastering, headless CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Analyze a file and print measurements (LUFS/TP/LRA/SNR) as JSON.
    Analyze {
        input: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Master a file: denoise + adaptive level + loudness + true-peak limit, then
    /// encode to whatever format `-o`'s extension implies (wav/mp3/flac/opus/ogg/m4a/
    /// aac/m4b).
    #[command(after_help = PRESET_HELP)]
    Master {
        input: PathBuf,
        /// Output path; format is inferred from the extension.
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long, value_parser = ["fast", "standard", "studio"])]
        tier: Option<String>,
        /// Bitrate in kbps for lossy formats (mp3/opus/ogg/m4a/aac/m4b). Ignored for
        /// wav/flac.
        #[arg(long)]
        bitrate: Option<u32>,
        /// Write a report to this path — the extension picks the shape. `.json` writes
        /// the raw MasterReport JSON verbatim (the eval harness's machine-readable
        /// contract, `eval/anvil_cli.py::run_master`); any other extension, or none,
        /// writes the human HTML+PDF compliance document instead (extension optional;
        /// the PDF is written alongside with a `.pdf` extension).
        #[arg(long)]
        report: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Transcribe a file to SRT/VTT/JSON, optionally with diarization.
    Transcribe {
        input: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        srt: bool,
        #[arg(long)]
        vtt: bool,
        #[arg(long)]
        diarize: bool,
        #[arg(long)]
        json: bool,
    },
    /// Batch-master a folder (04 §S4), optionally watching it for new files (04 §S5:
    /// "`anvil batch --watch` = S5 without GUI").
    #[command(after_help = PRESET_HELP)]
    Batch {
        dir: PathBuf,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long, value_parser = ["fast", "standard", "studio"])]
        tier: Option<String>,
        /// Output directory (default: `<dir>/mastered`).
        #[arg(short = 'o', long = "out")]
        out_dir: Option<PathBuf>,
        /// Recurse into subfolders (back-catalog mode) instead of just the top level.
        #[arg(long)]
        recursive: bool,
        /// Mirror input subfolders under the output directory. Only meaningful with
        /// `--recursive`.
        #[arg(long)]
        preserve_structure: bool,
        /// Keep running and master new files as they land (04 §S5).
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage model packs.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
}

#[derive(Subcommand)]
enum ModelsAction {
    /// List installed and available model packs.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Download a model pack (hash-verified, resumable).
    Pull {
        pack: String,
        #[arg(long)]
        json: bool,
    },
}

/// Print `error: {msg}` to stderr and return `code` — the shared unhappy-path helper
/// every command below funnels through, so exit codes stay consistent.
fn fail(code: u8, msg: &str) -> u8 {
    eprintln!("error: {msg}");
    code
}

// ---- Error classification ------------------------------------------------------------------

/// Map a decode/media failure onto the stable exit-code contract: anything that means
/// "this input can't be read" is `EXIT_BAD_INPUT`; sidecar/IO/resample plumbing
/// failures are `EXIT_INTERNAL`.
fn classify_media_error(e: &anvil_media::MediaError) -> u8 {
    use anvil_media::MediaError;
    match e {
        MediaError::UnsupportedFormat(_)
        | MediaError::NoAudioTrack(_)
        | MediaError::Decode(_)
        // A bad clip spec (range outside the audio, unknown encoder forced, …) is the
        // caller's input, not an internal fault.
        | MediaError::InvalidClip(_) => EXIT_BAD_INPUT,
        MediaError::Io(_)
        | MediaError::Resample(_)
        | MediaError::SidecarNotFound(_)
        | MediaError::SidecarFailed(_)
        | MediaError::SidecarHashMismatch { .. }
        | MediaError::Metadata(_) => EXIT_INTERNAL,
    }
}

/// Map an `anvil_dsp` failure onto the stable exit-code contract.
fn classify_dsp_error(e: &anvil_dsp::DspError) -> u8 {
    use anvil_dsp::DspError;
    match e {
        DspError::Empty => EXIT_BAD_INPUT,
        DspError::Meter(_) | DspError::Sink(_) => EXIT_INTERNAL,
        DspError::Media(m) => classify_media_error(m),
    }
}

// ---- Preset resolution (replaces the old local `resolve_preset`/`preset_target_lufs`) -----

/// Alias -> shipped id. Only the *short* names live here; the shipped ids themselves
/// (and their hyphen/underscore/case variants) already resolve via
/// [`canonical_preset_id`]'s direct match against [`Preset::shipped`].
const PRESET_ALIASES: &[(&str, &str)] = &[
    ("stereo", preset::PODCAST_STEREO_ID),
    ("podcast", preset::PODCAST_STEREO_ID),
    ("mono", preset::PODCAST_MONO_ID),
    ("spotify", preset::SPOTIFY_YOUTUBE_ID),
    ("youtube", preset::SPOTIFY_YOUTUBE_ID),
    ("streaming", preset::SPOTIFY_YOUTUBE_ID),
    ("broadcast", preset::BROADCAST_EBU_ID),
    ("ebu", preset::BROADCAST_EBU_ID),
    ("r128", preset::BROADCAST_EBU_ID),
    ("acx", preset::AUDIOBOOK_ACX_ID),
    ("audiobook", preset::AUDIOBOOK_ACX_ID),
    ("voice", preset::VOICE_MEMO_CLEANUP_ID),
    ("voice-memo", preset::VOICE_MEMO_CLEANUP_ID),
    ("music", preset::MUSIC_HEAVY_SHOW_ID),
    ("music-heavy", preset::MUSIC_HEAVY_SHOW_ID),
];

/// Resolve a name to a shipped preset's stable id: an exact (case/`-`/`_`-insensitive)
/// match against one of the seven `anvil_project::preset::*_ID` ids, or one of the
/// short [`PRESET_ALIASES`]. `None` if nothing matches.
fn canonical_preset_id(name: &str) -> Option<&'static str> {
    let normalized = name.to_lowercase().replace('_', "-");
    for shipped in Preset::shipped() {
        if shipped.id.replace('_', "-") == normalized {
            return Some(shipped.id);
        }
    }
    PRESET_ALIASES
        .iter()
        .find(|(alias, _)| *alias == normalized)
        .map(|(_, id)| *id)
}

/// Resolve `--preset`/`--tier` into a concrete [`Preset`] plus the canonical shipped id
/// it came from (needed later for the compliance report's ACX detection). `None` for
/// `name` defaults to `podcast_stereo`. `Err` carries the exact string the caller
/// passed, for the "unknown preset" error message.
fn resolve_preset(
    name: Option<&str>,
    tier: Option<&str>,
) -> Result<(Preset, &'static str), String> {
    let raw = name.unwrap_or(preset::PODCAST_STEREO_ID);
    let id = canonical_preset_id(raw).ok_or_else(|| raw.to_string())?;
    let mut resolved = Preset::by_id(id).expect("canonical_preset_id only returns shipped ids");
    if let Some(t) = tier {
        resolved.tier = match t.to_lowercase().as_str() {
            "fast" => Tier::Fast,
            "studio" => Tier::Studio,
            _ => Tier::Standard,
        };
    }
    Ok((resolved, id))
}

// ---- Output format resolution ----------------------------------------------------------------

/// What `master`/`batch` write to disk: either the local 16-bit WAV writer (no
/// `OutputFormat` variant covers WAV — [`anvil_media::encode`] only ever produces
/// compressed/lossless containers) or one of the real encoders.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OutTarget {
    Wav,
    Encoded(OutputFormat),
}

/// Infer the output format from `out`'s extension (case-insensitive).
fn resolve_output_format(out: &Path) -> Result<OutTarget, String> {
    let ext = out
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .ok_or_else(|| {
            format!(
                "output path {} has no extension — expected one of: wav, mp3, flac, opus, ogg, m4a, aac, m4b",
                out.display()
            )
        })?;
    Ok(match ext.as_str() {
        "wav" | "wave" => OutTarget::Wav,
        "mp3" => OutTarget::Encoded(OutputFormat::Mp3),
        "flac" => OutTarget::Encoded(OutputFormat::Flac),
        "opus" => OutTarget::Encoded(OutputFormat::Opus),
        "ogg" | "oga" => OutTarget::Encoded(OutputFormat::Vorbis),
        "m4a" | "aac" => OutTarget::Encoded(OutputFormat::Aac),
        "m4b" => OutTarget::Encoded(OutputFormat::M4b),
        other => {
            return Err(format!(
                "unrecognized output extension \".{other}\" — expected one of: wav, mp3, flac, opus, ogg, m4a, aac, m4b"
            ))
        }
    })
}

/// `anvil analyze` — full analysis pass (loudness, true peak, SNR, clipping, speech/music,
/// stereo, …) as JSON. Delegates to the deterministic engine (`anvil_dsp::analyze`); the
/// JSON keeps `integrated_lufs`/`true_peak_dbtp`/`loudness_range_lu` at the top level so the
/// eval harness can cross-check against ffmpeg's `ebur128`.
fn cmd_analyze(input: &Path, json: bool) -> u8 {
    if !input.exists() {
        return fail(
            EXIT_BAD_INPUT,
            &format!("input not found: {}", input.display()),
        );
    }
    let report = match anvil_dsp::analyze(input) {
        Ok(r) => r,
        Err(e) => return fail(classify_dsp_error(&e), &format!("analyze failed: {e}")),
    };
    if json {
        let v = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("integrated loudness : {:.2} LUFS", report.integrated_lufs);
        println!("true peak           : {:.2} dBTP", report.true_peak_dbtp);
        println!("loudness range      : {:.2} LU", report.loudness_range_lu);
        println!("SNR                 : {:.1} dB", report.snr_db);
        println!(
            "speech / music      : {:.0}% / {:.0}%",
            report.speech_ratio * 100.0,
            report.music_ratio * 100.0
        );
    }
    EXIT_OK
}

/// What `--report` produced. Extension-dispatched in [`cmd_master`]: a `.json` path
/// gets the raw [`anvil_dsp::MasterReport`] — the eval harness's machine-readable
/// contract (`eval/anvil_cli.py::run_master` reads it back with `json.loads`, matched
/// against `MasterReport`'s own `#[derive(Serialize)]`, so the two can never drift
/// silently) — and every other extension keeps the pre-existing human HTML+PDF
/// compliance document (04 §S6). Both are legitimate, simultaneous consumers of one
/// flag; this is what used to be an unconditional HTML+PDF write regardless of what the
/// caller asked for.
enum ReportOutcome {
    /// The raw `MasterReport`, serialized verbatim to this `.json` path.
    Json(PathBuf),
    /// The human compliance document: HTML at `html`, PDF at `pdf`, plus whether the
    /// master passed every compliance check (`anvil_project::compliance::overall_pass`).
    Compliance {
        html: PathBuf,
        pdf: PathBuf,
        compliant: bool,
    },
}

/// `anvil master` — the one-click chain: analyze → auto-configured Chain v1 → mastered
/// audio, encoded to whatever format `out`'s extension implies. `--report`'s extension
/// picks what gets written: `.json` writes the raw `MasterReport` (the eval harness's
/// machine-readable contract — see [`ReportOutcome`]); anything else writes the human
/// HTML+PDF compliance document built from the same measurements (04 §S6 "Compliance
/// report").
#[allow(clippy::too_many_arguments)]
fn cmd_master(
    input: &Path,
    out: &Path,
    preset_name: Option<&str>,
    tier: Option<&str>,
    bitrate_kbps: Option<u32>,
    report_path: Option<&Path>,
    json: bool,
) -> u8 {
    if !input.exists() {
        return fail(
            EXIT_BAD_INPUT,
            &format!("input not found: {}", input.display()),
        );
    }

    let (preset, preset_id) = match resolve_preset(preset_name, tier) {
        Ok(v) => v,
        Err(bad) => {
            return fail(
                EXIT_MISSING,
                &format!("unknown preset '{bad}' — run `anvil master --help` for the shipped ids"),
            )
        }
    };

    let out_target = match resolve_output_format(out) {
        Ok(t) => t,
        Err(msg) => return fail(EXIT_BAD_INPUT, &msg),
    };
    warn_if_bitrate_ignored(out_target, bitrate_kbps);

    // Make sure the output directory exists before the sink opens/spawns onto it.
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return fail(
                    EXIT_INTERNAL,
                    &format!("could not create {}: {e}", parent.display()),
                );
            }
        }
    }

    // Stream the master straight into the output sink — the whole file is never resident, so a
    // multi-hour master stays under the 06 §4 RAM budget (M5).
    let mut sink: Box<dyn anvil_dsp::BlockSink> = match out_target {
        OutTarget::Wav => Box::new(WavStreamSink::new(out)),
        OutTarget::Encoded(format) => match EncodedStreamSink::new(format, bitrate_kbps, out) {
            Ok(s) => Box::new(s),
            Err(e) => {
                return fail(
                    EXIT_INTERNAL,
                    &format!("could not start {} encoder: {e}", format.extension()),
                )
            }
        },
    };
    let result = match anvil_dsp::master_to_file(input, &preset, preset.tier, sink.as_mut()) {
        Ok(r) => r,
        Err(e) => return fail(classify_dsp_error(&e), &format!("master failed: {e}")),
    };

    let mut report_outcome = None;
    if let Some(rp) = report_path {
        let is_json_report = rp
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref()
            == Some("json");
        if is_json_report {
            // The eval harness's contract (`eval/anvil_cli.py::run_master`, `eval/run.py`'s
            // `master-eval`): the raw MasterReport, not the human compliance document below —
            // this is what M1 emitted before M2's compliance report took the flag over
            // unconditionally, restored here as an extension-dispatched sibling instead of a
            // replacement.
            if let Some(parent) = rp.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return fail(
                            EXIT_INTERNAL,
                            &format!("could not create {}: {e}", parent.display()),
                        );
                    }
                }
            }
            let text =
                serde_json::to_string_pretty(&result.report).unwrap_or_else(|_| "null".to_string());
            if let Err(e) = std::fs::write(rp, text) {
                return fail(
                    EXIT_INTERNAL,
                    &format!("could not write report {}: {e}", rp.display()),
                );
            }
            report_outcome = Some(ReportOutcome::Json(rp.to_path_buf()));
        } else {
            let html_path = if rp.extension().is_some() {
                rp.to_path_buf()
            } else {
                rp.with_extension("html")
            };
            let pdf_path = html_path.with_extension("pdf");
            let compliance = build_compliance_input(input, out, &preset, preset_id, &result);
            if let Err(e) =
                anvil_project::compliance::write_reports(&compliance, &html_path, &pdf_path)
            {
                return fail(
                    EXIT_INTERNAL,
                    &format!("could not write compliance report: {e}"),
                );
            }
            report_outcome = Some(ReportOutcome::Compliance {
                html: html_path,
                pdf: pdf_path,
                compliant: compliance.overall_pass(),
            });
        }
    }

    if json {
        let mut v = serde_json::to_value(&result.report).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "output".into(),
                serde_json::json!(out.display().to_string()),
            );
            obj.insert("preset_id".into(), serde_json::json!(preset_id));
            match &report_outcome {
                Some(ReportOutcome::Json(path)) => {
                    obj.insert(
                        "report".into(),
                        serde_json::json!({ "json": path.display().to_string() }),
                    );
                }
                Some(ReportOutcome::Compliance {
                    html,
                    pdf,
                    compliant,
                }) => {
                    obj.insert(
                        "report".into(),
                        serde_json::json!({
                            "html": html.display().to_string(),
                            "pdf": pdf.display().to_string(),
                            "compliant": compliant,
                        }),
                    );
                }
                None => {}
            }
        }
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("mastered  {}  ->  {}", input.display(), out.display());
        println!(
            "loudness  {:.2} -> {:.2} LUFS (target {:.1})   true peak {:.2} dBTP",
            result.report.before.integrated_lufs,
            result.report.after.integrated_lufs,
            preset.target_lufs,
            result.report.after.true_peak_dbtp,
        );
        match &report_outcome {
            Some(ReportOutcome::Json(path)) => {
                println!("report    {}  (MasterReport JSON)", path.display());
            }
            Some(ReportOutcome::Compliance {
                html,
                pdf,
                compliant,
            }) => {
                println!(
                    "report    {}  (+ {})   {}",
                    html.display(),
                    pdf.display(),
                    if *compliant {
                        "COMPLIANT"
                    } else {
                        "NOT COMPLIANT"
                    }
                );
            }
            None => {}
        }
    }
    EXIT_OK
}

fn warn_if_bitrate_ignored(target: OutTarget, bitrate_kbps: Option<u32>) {
    if bitrate_kbps.is_none() {
        return;
    }
    match target {
        OutTarget::Wav => eprintln!("note: --bitrate is ignored for wav output"),
        OutTarget::Encoded(format) if format.is_lossless() => {
            eprintln!(
                "note: --bitrate is ignored for lossless {} output",
                format.extension()
            )
        }
        OutTarget::Encoded(_) => {}
    }
}

/// Incremental 16-bit WAV sink: the streaming master hands it blocks and it quantizes and writes
/// them straight to disk (dither is applied in-chain for 16-bit). The `hound` writer is opened
/// lazily on the first block so the header's channel count matches the (possibly downmixed)
/// output.
struct WavStreamSink {
    path: PathBuf,
    writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
}

impl WavStreamSink {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            writer: None,
        }
    }
}

impl BlockSink for WavStreamSink {
    fn write(&mut self, block: &AudioBuffer) -> Result<(), anvil_dsp::DspError> {
        if block.frames() == 0 {
            return Ok(());
        }
        if self.writer.is_none() {
            let spec = hound::WavSpec {
                channels: block.channel_count().max(1) as u16,
                sample_rate: block.sample_rate(),
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let w = hound::WavWriter::create(&self.path, spec)
                .map_err(|e| anvil_dsp::DspError::Sink(e.to_string()))?;
            self.writer = Some(w);
        }
        let w = self.writer.as_mut().expect("writer opened above");
        let ch = block.channel_count();
        for f in 0..block.frames() {
            for c in 0..ch {
                let s = block.channel(c)[f];
                let q = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                w.write_sample(q)
                    .map_err(|e| anvil_dsp::DspError::Sink(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), anvil_dsp::DspError> {
        if let Some(w) = self.writer.take() {
            w.finalize()
                .map_err(|e| anvil_dsp::DspError::Sink(e.to_string()))?;
        }
        Ok(())
    }
}

/// Incremental ffmpeg-sidecar sink: wraps [`anvil_media::StreamEncoder`] so mastered blocks are
/// piped to ffmpeg as they are produced (mp3/flac/opus/ogg/m4a/aac/m4b) — the whole output is
/// never resident.
struct EncodedStreamSink {
    enc: Option<StreamEncoder>,
}

impl EncodedStreamSink {
    fn new(format: OutputFormat, bitrate_kbps: Option<u32>, path: &Path) -> Result<Self, String> {
        let spec = match bitrate_kbps {
            Some(kbps) => OutputSpec::new(format).with_bitrate(kbps),
            None => OutputSpec::new(format),
        };
        let enc = StreamEncoder::new(spec, path).map_err(|e| e.to_string())?;
        Ok(Self { enc: Some(enc) })
    }
}

impl BlockSink for EncodedStreamSink {
    fn write(&mut self, block: &AudioBuffer) -> Result<(), anvil_dsp::DspError> {
        if let Some(enc) = self.enc.as_mut() {
            enc.write_block(block)
                .map_err(|e| anvil_dsp::DspError::Sink(e.to_string()))?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), anvil_dsp::DspError> {
        if let Some(enc) = self.enc.take() {
            enc.finish()
                .map_err(|e| anvil_dsp::DspError::Sink(e.to_string()))?;
        }
        Ok(())
    }
}

/// Build a [`ComplianceInput`] straight from the streaming [`anvil_dsp::StreamMasterResult`]:
/// `before`/`after` are the chain's own loudness snapshots (compliance.rs's contract —
/// "guaranteed to equal what `anvil analyze` reports"), and the ACX-only RMS/noise-floor
/// measurements are computed on the actual rendered buffer, only when the ACX preset was
/// used (avoids a second full-file pass otherwise).
fn build_compliance_input(
    input: &Path,
    out: &Path,
    preset: &Preset,
    preset_id: &str,
    result: &anvil_dsp::StreamMasterResult,
) -> ComplianceInput {
    let report = &result.report;
    let is_acx = preset_id == preset::AUDIOBOOK_ACX_ID;
    // The mastered buffer is never resident on the streaming path, so the ACX RMS comes from the
    // streamed accumulator and the ACX noise floor from a streaming re-analysis of the output.
    let (rms_dbfs_out, noise_floor_dbfs_out) = if is_acx {
        let noise_floor = anvil_dsp::analyze(out).ok().map(|a| a.noise_floor_dbfs);
        (Some(result.out_rms_dbfs), noise_floor)
    } else {
        (None, None)
    };

    ComplianceInput {
        source_file: input.display().to_string(),
        output_file: out.display().to_string(),
        preset: preset.clone(),
        preset_id: Some(preset_id.to_string()),
        duration_secs: result.out_frames as f64 / f64::from(result.sample_rate.max(1)),
        sample_rate: result.sample_rate,
        channels: result.out_channels as u32,
        before: LoudnessMeasurement {
            integrated_lufs: report.before.integrated_lufs,
            true_peak_dbtp: report.before.true_peak_dbtp,
            loudness_range_lu: report.before.loudness_range_lu,
        },
        after: LoudnessMeasurement {
            integrated_lufs: report.after.integrated_lufs,
            true_peak_dbtp: report.after.true_peak_dbtp,
            loudness_range_lu: report.after.loudness_range_lu,
        },
        rms_dbfs_out,
        noise_floor_dbfs_out,
        modules: report
            .modules
            .iter()
            .map(|m| {
                if m.engaged {
                    ModuleDecision::applied(m.name.clone(), m.detail.clone())
                } else {
                    ModuleDecision::bypassed(m.name.clone(), m.detail.clone())
                }
            })
            .collect(),
        chain_version: report.chain_version,
    }
}

// ---- batch / watch --------------------------------------------------------------------------

/// `anvil batch` — drives [`anvil_batch::BatchQueue`] over a folder and, with `--watch`,
/// hands the same queue to [`WatchService`] (04 §CLI: "`anvil batch --watch` = S5
/// without GUI"). Output stays 16-bit WAV — `anvil_batch::queue::render_job` is the
/// interim WAV-only encoder seam (see that module's doc comment); swapping it to the
/// full format matrix is an `anvil-batch` change, out of this crate's scope.
#[allow(clippy::too_many_arguments)]
fn cmd_batch(
    dir: &Path,
    preset_name: Option<&str>,
    tier: Option<&str>,
    out_dir: Option<&Path>,
    recursive: bool,
    preserve_structure: bool,
    watch: bool,
    json: bool,
) -> u8 {
    let (preset, _preset_id) = match resolve_preset(preset_name, tier) {
        Ok(v) => v,
        Err(bad) => {
            return fail(
                EXIT_MISSING,
                &format!("unknown preset '{bad}' — run `anvil batch --help` for the shipped ids"),
            )
        }
    };

    let output_dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dir.join("mastered"));
    let mut settings = OutputSettings::new(output_dir.clone());
    if preserve_structure {
        settings = settings.preserving_structure();
    }

    let queue = match BatchQueue::new() {
        Ok(q) => Arc::new(q),
        Err(e) => {
            return fail(
                EXIT_INTERNAL,
                &format!("could not start the batch queue: {e}"),
            )
        }
    };

    if watch {
        return cmd_batch_watch(&queue, dir, preset, &output_dir, json);
    }

    if !dir.is_dir() {
        return fail(
            EXIT_BAD_INPUT,
            &format!("not a directory: {}", dir.display()),
        );
    }

    let ids = if recursive {
        match queue.submit_folder(dir, preset.clone(), preset.tier, &settings) {
            Ok(ids) => ids,
            Err(e) => {
                return fail(
                    EXIT_BAD_INPUT,
                    &format!("could not scan {}: {e}", dir.display()),
                )
            }
        }
    } else {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                return fail(
                    EXIT_BAD_INPUT,
                    &format!("could not read {}: {e}", dir.display()),
                )
            }
        };
        let files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && anvil_batch::catalog::is_supported(p))
            .collect();
        queue.submit_files(files, preset.clone(), preset.tier, &settings)
    };

    if ids.is_empty() {
        eprintln!(
            "no supported files found in {} ({}recursive)",
            dir.display(),
            if recursive { "" } else { "non-" }
        );
    }

    stream_batch_progress(&queue);

    let snapshot = queue.snapshot();
    let succeeded = snapshot
        .iter()
        .filter(|s| s.state == BatchItemState::Done)
        .count();
    let failed = snapshot
        .iter()
        .filter(|s| s.state == BatchItemState::Failed)
        .count();

    if json {
        print_batch_summary_json("done", &snapshot);
    } else {
        println!(
            "batch complete: {succeeded} succeeded, {failed} failed, {} total -> {}",
            snapshot.len(),
            output_dir.display()
        );
        for item in snapshot
            .iter()
            .filter(|s| s.state == BatchItemState::Failed)
        {
            println!(
                "  FAILED  {}: {}",
                item.input.display(),
                item.error.as_deref().unwrap_or("unknown error")
            );
        }
    }

    if failed > 0 {
        EXIT_INTERNAL
    } else {
        EXIT_OK
    }
}

/// Poll the queue and print a `\r`-updating progress line to stderr until every
/// submitted job reaches a terminal state.
fn stream_batch_progress(queue: &BatchQueue) {
    loop {
        let snapshot = queue.snapshot();
        let terminal = snapshot.iter().all(|s| {
            matches!(
                s.state,
                BatchItemState::Done | BatchItemState::Failed | BatchItemState::Cancelled
            )
        });
        eprint!(
            "\rbatch  {:>5.1}%  ({} files)",
            queue.overall_progress() * 100.0,
            snapshot.len()
        );
        let _ = std::io::stderr().flush();
        if terminal {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    eprintln!();
}

fn print_batch_summary_json(status: &str, snapshot: &[anvil_batch::BatchItemStatus]) {
    let succeeded = snapshot
        .iter()
        .filter(|s| s.state == BatchItemState::Done)
        .count();
    let failed = snapshot
        .iter()
        .filter(|s| s.state == BatchItemState::Failed)
        .count();
    let items: Vec<serde_json::Value> = snapshot
        .iter()
        .map(|s| {
            serde_json::json!({
                "input": s.input.display().to_string(),
                "output": s.output.display().to_string(),
                "state": format!("{:?}", s.state).to_lowercase(),
                "error": s.error,
            })
        })
        .collect();
    let payload = serde_json::json!({
        "status": status,
        "total": snapshot.len(),
        "succeeded": succeeded,
        "failed": failed,
        "items": items,
    });
    println!("{}", serde_json::to_string_pretty(&payload).unwrap());
}

/// `anvil batch --watch` — one [`WatchRule`] over `dir`, run forever (04 §S5: new files
/// only, back-catalog is the separate non-watch mode above). Ends when the process is
/// killed; there is no interactive Ctrl-C handler in this build (see `EXIT_CANCELLED`'s
/// doc comment), so this never returns — its divergent `loop` still type-checks as `u8`.
fn cmd_batch_watch(
    queue: &Arc<BatchQueue>,
    dir: &Path,
    preset: Preset,
    output_dir: &Path,
    json: bool,
) -> u8 {
    let service = WatchService::new(Arc::clone(queue));
    let tier = preset.tier;
    service.add_rule(WatchRule::new(dir, preset, tier, output_dir));

    if !json {
        eprintln!(
            "watching {}  ->  {}   (new files only; ctrl-c to stop)",
            dir.display(),
            output_dir.display()
        );
    }

    let mut reported: usize = 0;
    let mut last_error: Option<String> = None;
    loop {
        service.retry_unreachable();
        for status in service.list_rules() {
            if status.error != last_error {
                if let Some(err) = &status.error {
                    eprintln!("watch rule error: {err}");
                }
                last_error = status.error.clone();
            }
        }

        let snapshot = queue.snapshot();
        if !json {
            for item in snapshot.iter().skip(reported) {
                println!(
                    "{:<9} {} -> {}",
                    format!("{:?}", item.state).to_lowercase(),
                    item.input.display(),
                    item.output.display()
                );
            }
        }
        reported = snapshot.len();

        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---- models -----------------------------------------------------------------------------------

/// `anvil models list` — real, live model-pack state assembled from the engine catalogs (no
/// static milestone stubs): RNNoise is compiled into the binary; the whisper packs come from
/// `anvil_asr::KNOWN_MODELS` with installed-state via `anvil_asr::locate_model`; the Qwen
/// shownotes packs from `anvil_llm` with installed-state via `anvil_llm::locate_model`.
fn cmd_models_list(json: bool) -> u8 {
    struct Row {
        id: String,
        name: String,
        kind: &'static str,
        installed: bool,
        note: &'static str,
    }

    let mut rows = vec![Row {
        id: "rnnoise".into(),
        name: "RNNoise (fast-tier denoise)".into(),
        kind: "denoise",
        installed: true,
        note: "built into the binary — nothing to download",
    }];
    // The multilingual whisper packs the CLI can pull (the `.en` variants stay pullable by their
    // catalog id even though the list stays short). Installed-state is live from disk.
    for pack in anvil_asr::known_models().iter().filter(|m| m.multilingual) {
        rows.push(Row {
            id: format!("whisper-{}", pack.id),
            name: format!("Whisper — {}", pack.display_name),
            kind: "asr",
            installed: anvil_asr::locate_model(pack.id).is_some(),
            note: "whisper.cpp ggml weights (MIT) — real hash-verified download",
        });
    }
    for id in [anvil_llm::DEFAULT_MODEL_ID, anvil_llm::LOW_RAM_MODEL_ID] {
        let name = anvil_llm::model::find_pack(id)
            .map(|p| p.display_name.to_string())
            .unwrap_or_else(|| id.to_string());
        rows.push(Row {
            id: id.to_string(),
            name: format!("{name} (shownotes)"),
            kind: "llm",
            installed: anvil_llm::locate_model(id).is_some(),
            note: "optional — install from the desktop app's Models screen",
        });
    }

    if json {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "name": r.name,
                    "kind": r.kind,
                    "status": if r.installed { "installed" } else { "available" },
                    "note": r.note,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "packs": items })).unwrap()
        );
    } else {
        for r in &rows {
            let status = if r.installed {
                "installed"
            } else {
                "available, not installed"
            };
            println!("{:<28} {:<34} [{status}]  {}", r.id, r.name, r.note);
        }
    }
    EXIT_OK
}

/// `anvil models pull <pack>` — a **real** hash-verified download for the whisper packs (shared
/// byte-for-byte with the desktop Models screen via `anvil_models`), a no-op success for the
/// compiled-in RNNoise, and an honest "install from the desktop app" for the multi-file Qwen
/// shownotes packs (whose CLI download isn't wired yet). Whisper weights land in the per-user,
/// always-writable `anvil_models::models_dir()` — never inside a signed `.app` bundle.
fn cmd_models_pull(pack: &str, json: bool) -> u8 {
    if pack == "rnnoise" {
        emit_pull_status(
            json,
            "installed",
            pack,
            "rnnoise is compiled into the binary — nothing to download.",
            None,
        );
        return EXIT_OK;
    }
    if anvil_models::is_whisper_pack(pack) {
        return pull_whisper(pack, json);
    }
    if anvil_llm::model::find_pack(pack).is_some() {
        let msg = format!(
            "{pack} is a shownotes LLM pack — install it from the desktop app's Models screen \
             (the multi-file LLM download isn't wired into the CLI yet)."
        );
        if json {
            emit_pull_status(true, "unavailable", pack, &msg, None);
            return EXIT_MISSING;
        }
        return fail(EXIT_MISSING, &msg);
    }
    fail(
        EXIT_MISSING,
        &format!("unknown model pack '{pack}' — run `anvil models list`"),
    )
}

/// The whisper download itself: skip if already installed, else stream + sha1-verify via the
/// shared `anvil_models` engine with a throttled progress line, and report the install.
fn pull_whisper(pack: &str, json: bool) -> u8 {
    let catalog_id = pack.strip_prefix("whisper-").unwrap_or(pack);
    if let Some(path) = anvil_asr::locate_model(catalog_id) {
        emit_pull_status(
            json,
            "installed",
            pack,
            &format!("{pack} is already installed ({}).", path.display()),
            Some(path.display().to_string()),
        );
        return EXIT_OK;
    }

    let dir = anvil_models::models_dir();
    if !json {
        eprintln!("downloading {pack} → {}", dir.display());
    }
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut last_pct: u64 = 0;
    let outcome =
        anvil_models::fetch_whisper_model(pack, &dir, &cancel, |downloaded, total, tick| {
            if json {
                return;
            }
            match tick {
                anvil_models::Tick::Downloading => {
                    let pct = downloaded
                        .saturating_mul(100)
                        .checked_div(total)
                        .unwrap_or(0);
                    if pct >= last_pct + 5 {
                        last_pct = pct;
                        eprintln!(
                            "  {pct:>3}%  ({} / {} MB)",
                            downloaded / 1_000_000,
                            total / 1_000_000
                        );
                    }
                }
                anvil_models::Tick::Verifying => eprintln!("  verifying checksum…"),
            }
        });

    match outcome {
        Ok(anvil_models::Outcome::Installed(installed)) => {
            emit_pull_status(
                json,
                "installed",
                pack,
                &format!(
                    "installed {pack} → {} ({} MB, sha1 {})",
                    installed.path.display(),
                    installed.size_bytes / 1_000_000,
                    installed.sha1
                ),
                Some(installed.path.display().to_string()),
            );
            EXIT_OK
        }
        Ok(anvil_models::Outcome::Paused) => fail(
            EXIT_INTERNAL,
            &format!("the {pack} download did not complete"),
        ),
        Err(e) => fail(EXIT_INTERNAL, &format!("could not download {pack}: {e}")),
    }
}

/// Emit a `models pull` result as pretty JSON (when `json`) or a plain line, so the two output
/// modes stay in lockstep.
fn emit_pull_status(json: bool, status: &str, pack: &str, detail: &str, path: Option<String>) {
    if json {
        let mut obj = serde_json::json!({ "status": status, "pack": pack, "detail": detail });
        if let Some(p) = path {
            obj["path"] = serde_json::Value::String(p);
        }
        println!("{}", serde_json::to_string_pretty(&obj).unwrap());
    } else {
        println!("{detail}");
    }
}

/// `anvil transcribe` — whisper.cpp word-level transcription (M3). Stages a 16 kHz mono WAV
/// (what whisper reads best), runs the sidecar, and writes SRT/VTT and/or prints TXT/JSON.
fn cmd_transcribe(
    input: &Path,
    model: Option<&str>,
    srt: bool,
    vtt: bool,
    diarize: bool,
    json: bool,
) -> u8 {
    let buffer = match anvil_media::decode_to_buffer(input) {
        Ok(b) => b,
        Err(e) => {
            return fail(
                EXIT_BAD_INPUT,
                &format!("could not read {}: {e}", input.display()),
            )
        }
    };
    let staged = std::env::temp_dir().join(format!("anvil-asr-{}.wav", std::process::id()));
    if let Err(e) = write_wav_16k_mono(&staged, &buffer) {
        return fail(EXIT_INTERNAL, &format!("could not stage audio: {e}"));
    }

    let mut opts = anvil_asr::TranscribeOptions::default();
    if let Some(m) = model {
        let as_path = Path::new(m);
        opts.model = if as_path.is_file() {
            Some(as_path.to_path_buf())
        } else {
            anvil_asr::locate_model(m)
        };
        if opts.model.is_none() {
            let _ = std::fs::remove_file(&staged);
            return fail(
                EXIT_MISSING,
                &format!("model not found: {m} (pass a .bin path or install the pack)"),
            );
        }
    }

    let mut transcript = match anvil_asr::transcribe(&staged, &opts) {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            let code = if e.to_string().to_lowercase().contains("not found") {
                EXIT_MISSING
            } else {
                EXIT_INTERNAL
            };
            return fail(code, &format!("transcribe failed: {e}"));
        }
    };
    // Diarization runs on the same staged 16 kHz mono WAV; degrade cleanly (like the desktop) when
    // the sherpa binary/models aren't present rather than failing the whole transcribe.
    if diarize {
        match anvil_asr::diarize(&staged, &anvil_asr::DiarizeOptions::default()) {
            Ok(diar) => {
                anvil_asr::assign_speakers(&mut transcript, &diar);
                eprintln!("(diarized: {} speaker(s))", diar.speakers.len());
            }
            Err(e) => {
                eprintln!("note: diarization unavailable ({e}); continuing without speaker labels")
            }
        }
    }
    let _ = std::fs::remove_file(&staged);

    let stem = input.with_extension("");
    let mut wrote = false;
    if srt {
        let p = stem.with_extension("srt");
        if let Err(e) = std::fs::write(&p, build_subtitles(&transcript, true)) {
            return fail(EXIT_INTERNAL, &format!("write {}: {e}", p.display()));
        }
        println!("wrote {}", p.display());
        wrote = true;
    }
    if vtt {
        let p = stem.with_extension("vtt");
        if let Err(e) = std::fs::write(&p, build_subtitles(&transcript, false)) {
            return fail(EXIT_INTERNAL, &format!("write {}: {e}", p.display()));
        }
        println!("wrote {}", p.display());
        wrote = true;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&transcript).unwrap_or_default()
        );
    } else if !wrote {
        println!("{}", transcript.text());
    }
    eprintln!(
        "({} words, language {})",
        transcript.words.len(),
        if transcript.language.is_empty() {
            "?"
        } else {
            transcript.language.as_str()
        }
    );
    EXIT_OK
}

/// Downmix to mono and resample to 16 kHz (what whisper.cpp wants), then write 16-bit PCM WAV.
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

/// Render a transcript's segments as SRT (`comma = true`) or WebVTT (`comma = false`).
fn build_subtitles(transcript: &anvil_asr::Transcript, comma: bool) -> String {
    let ts = |t: f64| -> String {
        let ms_total = (t.max(0.0) * 1000.0).round() as u64;
        let (h, m, s, ms) = (
            ms_total / 3_600_000,
            (ms_total % 3_600_000) / 60_000,
            (ms_total % 60_000) / 1000,
            ms_total % 1000,
        );
        let sep = if comma { ',' } else { '.' };
        format!("{h:02}:{m:02}:{s:02}{sep}{ms:03}")
    };
    let mut out = if comma {
        String::new()
    } else {
        String::from("WEBVTT\n\n")
    };
    for (i, seg) in transcript.segments.iter().enumerate() {
        if comma {
            out.push_str(&format!("{}\n", i + 1));
        }
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            ts(seg.start),
            ts(seg.end),
            seg.text.trim()
        ));
    }
    out
}

fn main() -> ExitCode {
    let code = match Cli::parse().command {
        Command::Analyze { input, json } => cmd_analyze(&input, json),
        Command::Master {
            input,
            out,
            preset,
            tier,
            bitrate,
            report,
            json,
        } => cmd_master(
            &input,
            &out,
            preset.as_deref(),
            tier.as_deref(),
            bitrate,
            report.as_deref(),
            json,
        ),
        Command::Transcribe {
            input,
            model,
            srt,
            vtt,
            diarize,
            json,
        } => cmd_transcribe(&input, model.as_deref(), srt, vtt, diarize, json),
        Command::Batch {
            dir,
            preset,
            tier,
            out_dir,
            recursive,
            preserve_structure,
            watch,
            json,
        } => cmd_batch(
            &dir,
            preset.as_deref(),
            tier.as_deref(),
            out_dir.as_deref(),
            recursive,
            preserve_structure,
            watch,
            json,
        ),
        Command::Models { action } => match action {
            ModelsAction::List { json } => cmd_models_list(json),
            ModelsAction::Pull { pack, json } => cmd_models_pull(&pack, json),
        },
    };
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_wav_named(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..48_000u32 {
            let s = (0.2 * ((i as f32) * 0.05).sin() * i16::MAX as f32) as i16;
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    fn fixture_wav(dir: &Path) -> PathBuf {
        fixture_wav_named(dir, "in.wav")
    }

    // ---- preset resolution ----

    #[test]
    fn canonical_preset_id_resolves_shipped_ids_in_any_separator_or_case() {
        assert_eq!(
            canonical_preset_id("podcast_stereo"),
            Some(preset::PODCAST_STEREO_ID)
        );
        assert_eq!(
            canonical_preset_id("podcast-stereo"),
            Some(preset::PODCAST_STEREO_ID)
        );
        assert_eq!(
            canonical_preset_id("PODCAST_STEREO"),
            Some(preset::PODCAST_STEREO_ID)
        );
        assert_eq!(
            canonical_preset_id("voice_memo_cleanup"),
            Some(preset::VOICE_MEMO_CLEANUP_ID)
        );
        assert_eq!(
            canonical_preset_id("music_heavy_show"),
            Some(preset::MUSIC_HEAVY_SHOW_ID)
        );
    }

    #[test]
    fn canonical_preset_id_resolves_friendly_aliases() {
        assert_eq!(canonical_preset_id("mono"), Some(preset::PODCAST_MONO_ID));
        assert_eq!(
            canonical_preset_id("spotify"),
            Some(preset::SPOTIFY_YOUTUBE_ID)
        );
        assert_eq!(canonical_preset_id("ACX"), Some(preset::AUDIOBOOK_ACX_ID));
        assert_eq!(canonical_preset_id("r128"), Some(preset::BROADCAST_EBU_ID));
        assert_eq!(
            canonical_preset_id("music-heavy"),
            Some(preset::MUSIC_HEAVY_SHOW_ID)
        );
    }

    #[test]
    fn canonical_preset_id_rejects_unknown_name() {
        assert_eq!(canonical_preset_id("bogus"), None);
    }

    #[test]
    fn resolve_preset_defaults_to_podcast_stereo() {
        let (preset, id) = resolve_preset(None, None).unwrap();
        assert_eq!(id, preset::PODCAST_STEREO_ID);
        assert_eq!(preset.target_lufs, -16.0);
        assert_eq!(preset.tier, Tier::Standard);
    }

    #[test]
    fn resolve_preset_applies_tier_override() {
        let (preset, id) = resolve_preset(Some("acx"), Some("studio")).unwrap();
        assert_eq!(id, preset::AUDIOBOOK_ACX_ID);
        assert_eq!(preset.tier, Tier::Studio);
        assert_eq!(preset.true_peak_ceiling_dbtp, -3.0);
    }

    #[test]
    fn resolve_preset_rejects_unknown_name() {
        assert_eq!(
            resolve_preset(Some("bogus"), None),
            Err("bogus".to_string())
        );
    }

    // ---- output format resolution ----

    #[test]
    fn resolve_output_format_maps_every_known_extension() {
        let cases: &[(&str, OutTarget)] = &[
            ("x.wav", OutTarget::Wav),
            ("x.WAV", OutTarget::Wav),
            ("x.wave", OutTarget::Wav),
            ("x.mp3", OutTarget::Encoded(OutputFormat::Mp3)),
            ("x.flac", OutTarget::Encoded(OutputFormat::Flac)),
            ("x.opus", OutTarget::Encoded(OutputFormat::Opus)),
            ("x.ogg", OutTarget::Encoded(OutputFormat::Vorbis)),
            ("x.m4a", OutTarget::Encoded(OutputFormat::Aac)),
            ("x.aac", OutTarget::Encoded(OutputFormat::Aac)),
            ("x.m4b", OutTarget::Encoded(OutputFormat::M4b)),
        ];
        for (name, expected) in cases {
            assert_eq!(
                resolve_output_format(Path::new(name)).unwrap(),
                *expected,
                "extension in {name}"
            );
        }
    }

    #[test]
    fn resolve_output_format_rejects_unknown_or_missing_extension() {
        assert!(resolve_output_format(Path::new("x.xyz")).is_err());
        assert!(resolve_output_format(Path::new("no-extension")).is_err());
    }

    // ---- exit codes: analyze ----

    #[test]
    fn cmd_analyze_missing_input_is_bad_input() {
        assert_eq!(
            cmd_analyze(Path::new("does-not-exist.wav"), false),
            EXIT_BAD_INPUT
        );
    }

    #[test]
    fn cmd_analyze_succeeds_on_a_real_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        assert_eq!(cmd_analyze(&input, true), EXIT_OK);
    }

    // ---- exit codes + behavior: master ----

    #[test]
    fn cmd_master_missing_input_is_bad_input() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.wav");
        let code = cmd_master(
            Path::new("does-not-exist.wav"),
            &out,
            None,
            None,
            None,
            None,
            false,
        );
        assert_eq!(code, EXIT_BAD_INPUT);
    }

    #[test]
    fn cmd_master_unknown_preset_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        let out = tmp.path().join("out.wav");
        let code = cmd_master(&input, &out, Some("bogus-preset"), None, None, None, false);
        assert_eq!(code, EXIT_MISSING);
    }

    #[test]
    fn cmd_master_unrecognized_output_extension_is_bad_input() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        let out = tmp.path().join("out.xyz");
        let code = cmd_master(&input, &out, None, None, None, None, false);
        assert_eq!(code, EXIT_BAD_INPUT);
    }

    #[test]
    fn cmd_master_writes_wav_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        let out = tmp.path().join("nested").join("out.wav");
        let code = cmd_master(&input, &out, None, None, None, None, false);
        assert_eq!(code, EXIT_OK);
        assert!(out.exists());
        assert!(std::fs::metadata(&out).unwrap().len() > 44); // more than just a WAV header
    }

    #[test]
    fn cmd_master_writes_html_and_pdf_compliance_report() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        let out = tmp.path().join("out.wav");
        let report = tmp.path().join("report.html");
        let code = cmd_master(&input, &out, Some("acx"), None, None, Some(&report), false);
        assert_eq!(code, EXIT_OK);
        assert!(report.exists());
        assert!(tmp.path().join("report.pdf").exists());
        let html = std::fs::read_to_string(&report).unwrap();
        assert!(html.contains("ACX submission checklist"));
    }

    #[test]
    fn cmd_master_report_path_without_extension_gets_html_and_pdf_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        let out = tmp.path().join("out.wav");
        let report = tmp.path().join("report");
        let code = cmd_master(&input, &out, None, None, None, Some(&report), false);
        assert_eq!(code, EXIT_OK);
        assert!(tmp.path().join("report.html").exists());
        assert!(tmp.path().join("report.pdf").exists());
    }

    /// Regression test for the CLI contract bug: `--report <path>.json` used to get the
    /// same unconditional HTML+PDF compliance writer as every other extension, so the
    /// eval harness's `json.loads(report_path)` (`eval/anvil_cli.py::run_master`) died on
    /// literal `<!doctype html>` bytes. A `.json` path must now get the raw `MasterReport`
    /// — the same shape `anvil-dsp`'s own contract test (`master_end_to_end.rs`) checks —
    /// not the compliance document.
    #[test]
    fn cmd_master_report_json_extension_writes_the_raw_master_report() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        let out = tmp.path().join("out.wav");
        let report = tmp.path().join("report.json");
        let code = cmd_master(&input, &out, None, None, None, Some(&report), false);
        assert_eq!(code, EXIT_OK);
        assert!(report.exists());
        // The HTML/PDF compliance siblings must NOT appear next to a `.json` report path.
        assert!(!tmp.path().join("report.html").exists());
        assert!(!tmp.path().join("report.pdf").exists());

        let text = std::fs::read_to_string(&report).unwrap();
        assert!(
            !text.trim_start().to_lowercase().starts_with("<!doctype"),
            "report.json contains HTML, not JSON: {}",
            &text[..text.len().min(80)]
        );

        // Must parse the same way `eval/anvil_cli.py::run_master` parses it
        // (`json.loads(rp.read_text())`).
        let v: serde_json::Value = serde_json::from_str(&text)
            .expect("--report out.json must be valid, eval-harness-parseable JSON");

        // MasterReport's contract keys (anvil_dsp::chain::MasterReport, cross-checked by
        // anvil-dsp/tests/master_end_to_end.rs and mirrored in
        // eval/tests/test_anvil_cli.py::test_run_master_reads_back_report_json).
        for key in [
            "analysis",
            "before",
            "after",
            "preset",
            "tier",
            "chain_version",
            "modules",
            "health_card",
        ] {
            assert!(v.get(key).is_some(), "MasterReport JSON missing key {key}");
        }

        // Loudness values must be sane vs. the actual render: the default preset targets
        // -16 LUFS, and the two-pass loudness normalize's whole job is landing near it.
        let before_lufs = v["before"]["integrated_lufs"].as_f64().unwrap();
        let after_lufs = v["after"]["integrated_lufs"].as_f64().unwrap();
        assert!(before_lufs.is_finite() && after_lufs.is_finite());
        assert!(
            (after_lufs - (-16.0)).abs() < 3.0,
            "after.integrated_lufs {after_lufs} is not within 3 LU of the podcast-stereo \
             -16 LUFS default — loudness normalize looks broken, not just imprecise"
        );
        assert_eq!(v["tier"], "standard");
        assert!(
            v["chain_version"].as_u64().unwrap() > 0,
            "chain_version should be a real (nonzero) version, got {}",
            v["chain_version"]
        );
    }

    /// Exercises every real-encoder path through the ffmpeg sidecar (mp3/flac/opus/
    /// vorbis/aac/m4b) — requires `ANVIL_FFMPEG` to point at a real ffmpeg binary.
    #[test]
    fn cmd_master_encodes_every_compressed_format_via_ffmpeg_sidecar() {
        if std::env::var_os("ANVIL_FFMPEG").is_none() {
            eprintln!("skipping: ANVIL_FFMPEG not set");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let input = fixture_wav(tmp.path());
        for ext in ["mp3", "flac", "opus", "ogg", "m4a", "m4b"] {
            let out = tmp.path().join(format!("out.{ext}"));
            let code = cmd_master(&input, &out, None, None, Some(96), None, false);
            assert_eq!(code, EXIT_OK, "encoding .{ext} should succeed");
            assert!(out.exists(), ".{ext} output should exist");
            assert!(
                std::fs::metadata(&out).unwrap().len() > 0,
                ".{ext} output should be non-empty"
            );
        }
    }

    // ---- batch ----

    #[test]
    fn cmd_batch_rejects_a_non_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = fixture_wav(tmp.path());
        let code = cmd_batch(
            &not_a_dir,
            None,
            Some("fast"),
            None,
            false,
            false,
            false,
            false,
        );
        assert_eq!(code, EXIT_BAD_INPUT);
    }

    #[test]
    fn cmd_batch_unknown_preset_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let code = cmd_batch(
            tmp.path(),
            Some("bogus"),
            None,
            None,
            false,
            false,
            false,
            false,
        );
        assert_eq!(code, EXIT_MISSING);
    }

    #[test]
    fn cmd_batch_masters_flat_files_in_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_wav_named(tmp.path(), "a.wav");
        fixture_wav_named(tmp.path(), "b.wav");
        let code = cmd_batch(
            tmp.path(),
            None,
            Some("fast"),
            None,
            false,
            false,
            false,
            false,
        );
        assert_eq!(code, EXIT_OK);
        let out_dir = tmp.path().join("mastered");
        assert!(out_dir.join("a_mastered.wav").exists());
        assert!(out_dir.join("b_mastered.wav").exists());
    }

    #[test]
    fn cmd_batch_recursive_preserves_structure() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("season1")).unwrap();
        fixture_wav_named(&tmp.path().join("season1"), "ep1.wav");
        let out_dir = tmp.path().join("out");
        let code = cmd_batch(
            tmp.path(),
            None,
            Some("fast"),
            Some(&out_dir),
            true,
            true,
            false,
            false,
        );
        assert_eq!(code, EXIT_OK);
        assert!(out_dir.join("season1").join("ep1_mastered.wav").exists());
    }

    #[test]
    fn cmd_batch_non_recursive_ignores_subfolders() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("season1")).unwrap();
        fixture_wav_named(&tmp.path().join("season1"), "ep1.wav");
        fixture_wav_named(tmp.path(), "top.wav");
        let out_dir = tmp.path().join("out");
        let code = cmd_batch(
            tmp.path(),
            None,
            Some("fast"),
            Some(&out_dir),
            false,
            false,
            false,
            false,
        );
        assert_eq!(code, EXIT_OK);
        assert!(out_dir.join("top_mastered.wav").exists());
        assert!(!out_dir.join("ep1_mastered.wav").exists());
    }

    // ---- models ----

    #[test]
    fn cmd_models_list_is_ok() {
        assert_eq!(cmd_models_list(true), EXIT_OK);
        assert_eq!(cmd_models_list(false), EXIT_OK);
    }

    #[test]
    fn cmd_models_pull_rnnoise_is_a_noop_success() {
        assert_eq!(cmd_models_pull("rnnoise", false), EXIT_OK);
    }

    #[test]
    fn cmd_models_pull_recognizes_whisper_and_defers_the_llm() {
        // Whisper packs are real, hash-verified pulls now — recognized by the shared downloader.
        // (We assert recognition rather than call `cmd_models_pull`, which would fire the real
        // multi-hundred-MB network fetch; that end-to-end path is `anvil_models`' opt-in test.)
        assert!(anvil_models::is_whisper_pack("whisper-small"));
        assert!(anvil_models::is_whisper_pack("whisper-medium"));
        // The multi-file shownotes LLM is desktop-managed for now → a clean `EXIT_MISSING`, no
        // download attempted from the CLI.
        assert_eq!(
            cmd_models_pull(anvil_llm::DEFAULT_MODEL_ID, true),
            EXIT_MISSING
        );
    }

    #[test]
    fn cmd_models_pull_unknown_pack_is_missing() {
        assert_eq!(cmd_models_pull("does-not-exist", false), EXIT_MISSING);
    }

    // ---- exit code contract sanity ----

    #[test]
    fn exit_codes_match_the_stable_contract() {
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_BAD_INPUT, 2);
        assert_eq!(EXIT_MISSING, 3);
        assert_eq!(EXIT_CANCELLED, 4);
        assert_eq!(EXIT_INTERNAL, 5);
    }
}
