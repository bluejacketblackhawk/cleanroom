//! # anvil-dsp
//!
//! Deterministic, block-based DSP for the ANVIL mastering chain (spec: `handoff/03-DSP-SPEC.md`).
//! M1 ships the analysis pass (§1) and Chain v1 (§3 subset): DC/HPF (§4.1), AI denoise
//! (§4.4, via `anvil-ai`), adaptive leveler (§4.8), two-pass loudness normalize (§4.9),
//! true-peak limiter (§4.10), and dither (§4.11), plus the auto-decision (§2) and Health Card.
//! M3 added the speech-repair modules (de-hum §4.2, mouth de-click §4.3, breath §4.5,
//! de-esser §4.6, AutoEQ §4.7); M4 adds the repair pair de-clip + de-crackle (§4.3) and
//! **per-speaker leveling + Voice Memory** (§4.8 per-speaker mode, §4.7 — see [`speaker`]).
//!
//! Public entry points:
//! - [`analyze`] / [`analyze_buffer`] → an [`AnalysisReport`].
//! - [`master`] → a [`MasterResult`] (processed audio + a [`MasterReport`]).
//! - [`master_with_diarization`] → the same, with per-speaker leveling and Voice Memory.
//!
//! Every processor is **deterministic** (ADR-003): identical input + params ⇒ bit-identical
//! output. The only entropy anywhere is the dither, seeded from a content hash.

use std::path::Path;

use anvil_asr::Diarization;
use anvil_media::{decode_blocks, decode_to_buffer, AudioBuffer};
use anvil_project::{Preset, Tier};

pub mod analysis;
mod ar;
pub mod autoeq;
pub mod biquad;
pub mod breath;
pub mod chain;
pub mod declick;
pub mod declip;
pub mod deess;
pub mod dehum;
pub mod dither;
pub mod error;
mod hash;
pub mod hpf;
pub mod leveler;
pub mod limiter;
pub mod speaker;
pub mod stream;

pub use analysis::{AnalysisReport, Analyzer, Hum, SilenceRun, StereoStats};
pub use autoeq::{AutoEq, AutoEqConfig, BandFit, EqTarget};
pub use breath::{BreathConfig, BreathControl};
pub use chain::{
    auto_configure, auto_configure_with_diarization, build_master_report, run_front_stages, Chain,
    ChainConfig, HealthFinding, LoudnessSnapshot, MasterReport, ModuleReport, RenderOutcome,
};
pub use declick::{DeCrackle, DeCrackleConfig, MouthDeClick, MouthDeClickConfig};
pub use declip::{DeClip, DeClipConfig};
pub use deess::{DeEsser, DeEsserConfig};
pub use dehum::{DeHum, DeHumConfig};
pub use dither::{Dither, DitherConfig};
pub use error::DspError;
pub use hpf::{DcHpf, DcHpfConfig, HpfMode};
pub use leveler::{Leveler, LevelerConfig};
pub use limiter::{LimiterConfig, StreamingLimiter, TruePeakLimiter};
pub use speaker::{
    derive_profiles, MedianSource, SpeakerGain, SpeakerLeveler, SpeakerLevelingConfig,
    SpeakerProfile, VoiceMemory,
};
pub use stream::{master_to_file, BlockSink, StreamMasterResult};

// Re-export the denoiser so callers can reach it through the DSP crate's chain surface.
// `DenoiseTier` must come with it: without it a caller building its own chain (e.g.
// `anvil-multitrack`'s per-track front chain) can only reach `Denoiser::new`, and every tier
// then silently denoises identically.
pub use anvil_ai::{DenoiseConfig, DenoiseTier, Denoiser};

/// A streaming block processor. Modules declare their [`Processor::latency_samples`] so
/// the graph can latency-compensate and keep A/B sample-aligned (ADR-002).
pub trait Processor {
    /// Process one block of audio in place.
    fn process(&mut self, buffer: &mut AudioBuffer);

    /// Extra output latency in samples introduced by this processor. Default: none.
    fn latency_samples(&self) -> usize {
        0
    }

    /// Reset internal state to a clean start (e.g. before re-rendering).
    fn reset(&mut self) {}
}

/// Linear-gain processor — the canonical trivial [`Processor`] and a determinism smoke
/// test. Not part of the mastering chain by itself.
#[derive(Debug, Clone, Copy)]
pub struct Gain {
    /// Linear gain factor (1.0 = unity).
    pub linear: f32,
}

impl Gain {
    /// Construct from a decibel value.
    pub fn from_db(db: f32) -> Self {
        Self {
            linear: 10f32.powf(db / 20.0),
        }
    }
}

impl Processor for Gain {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        for channel in buffer.planar_mut() {
            for sample in channel.iter_mut() {
                *sample *= self.linear;
            }
        }
    }
}

/// The processed audio plus its report.
pub struct MasterResult {
    /// The mastered audio (planar f32 @ 48 kHz).
    pub audio: AudioBuffer,
    /// The full master report (analysis + before/after + modules + Health Card).
    pub report: MasterReport,
    /// Fresh Voice Memory profiles for this file's cast (§4.7) — empty unless the file was
    /// mastered with a diarization *and* profile derivation was asked for. The storage lane
    /// (`anvil-project`) persists these so the next episode recognizes the same voices.
    pub voice_memory: VoiceMemory,
}

/// Analyze a file: decode via `anvil-media` (streaming, one block at a time so a multi-hour
/// file never fully resides in RAM) and produce an [`AnalysisReport`].
pub fn analyze(input: &Path) -> Result<AnalysisReport, DspError> {
    let mut decoder = decode_blocks(input)?;
    let channels = decoder.channel_count().max(1);
    let mut analyzer = Analyzer::new(channels)?;
    for block in &mut decoder {
        let block = block?;
        analyzer.push(&block)?;
    }
    Ok(analyzer.finish(anvil_core::CHAIN_VERSION))
}

/// Analyze an already-decoded buffer. Feeds it through the same accumulator in
/// block-sized chunks so short-term/momentary maxima track properly.
pub fn analyze_buffer(buf: &AudioBuffer) -> AnalysisReport {
    let channels = buf.channel_count().max(1);
    let mut analyzer = match Analyzer::new(channels) {
        Ok(a) => a,
        // ebur128 only rejects invalid channel counts / rates, which we've guarded; if it
        // ever fails, fall back to a silent report rather than panicking.
        Err(_) => return empty_report(channels, buf.sample_rate()),
    };
    let frames = buf.frames();
    let mut start = 0;
    while start < frames {
        let end = (start + anvil_core::BLOCK_SAMPLES).min(frames);
        let chunk: Vec<Vec<f32>> = buf
            .planar()
            .iter()
            .map(|c| c[start..end].to_vec())
            .collect();
        let block = AudioBuffer::from_planar(chunk, buf.sample_rate());
        if analyzer.push(&block).is_err() {
            break;
        }
        start = end;
    }
    analyzer.finish(anvil_core::CHAIN_VERSION)
}

/// Master a file end-to-end: decode → analyze → auto-decide → run Chain v1 → report.
///
/// M1 decodes the whole file into RAM (the eval corpus clips are short; a streaming master
/// for multi-hour files is a later milestone). The output is the mastered [`AudioBuffer`]
/// plus a [`MasterReport`] whose JSON is the CLI/UI/eval contract.
pub fn master(input: &Path, preset: &Preset, tier: Tier) -> Result<MasterResult, DspError> {
    master_with_diarization(input, preset, tier, None, None)
}

/// Master a file **with speakers**: same path as [`master`], but a supplied [`Diarization`]
/// engages per-speaker leveling (03 §4.8) and any [`VoiceMemory`] profiles are applied (§4.7).
///
/// The returned [`MasterResult::voice_memory`] carries fresh profiles derived from this render,
/// for the storage lane to persist.
///
/// `diarization: None` is exactly [`master`] — the single-speaker path, untouched.
pub fn master_with_diarization(
    input: &Path,
    preset: &Preset,
    tier: Tier,
    diarization: Option<&Diarization>,
    memory: Option<&VoiceMemory>,
) -> Result<MasterResult, DspError> {
    let buf = decode_to_buffer(input)?;
    if buf.is_empty() {
        return Err(DspError::Empty);
    }
    let report = analyze_buffer(&buf);
    let mut config = auto_configure_with_diarization(&report, preset, tier, diarization, memory);
    // Learn while we work: a master with speakers also hands back the profiles for next time.
    if let Some(spk) = config.speaker.as_mut() {
        spk.profile_autoeq = config.autoeq.or(Some(AutoEqConfig::default()));
    }
    let mut chain = Chain::new(buf.sample_rate());
    let outcome = chain.render(&buf, &config);
    let master_report = build_master_report(report, &config, &outcome);
    Ok(MasterResult {
        audio: outcome.audio,
        report: master_report,
        voice_memory: outcome.voice_memory,
    })
}

/// A safe all-silent report for degenerate inputs (keeps `analyze_buffer` infallible).
fn empty_report(channels: usize, sample_rate: u32) -> AnalysisReport {
    AnalysisReport {
        integrated_lufs: -120.0,
        true_peak_dbtp: -120.0,
        loudness_range_lu: 0.0,
        short_term_max_lufs: -120.0,
        momentary_max_lufs: -120.0,
        duration_secs: 0.0,
        sample_rate,
        channels: channels as u32,
        noise_floor_dbfs: -120.0,
        snr_db: 0.0,
        clipping_regions: 0,
        worst_clip_secs: None,
        dc_offset: vec![0.0; channels],
        hum: None,
        rt60_secs: None,
        reverb_bucket: "ok".into(),
        bandwidth_hz: 0.0,
        speech_ratio: 0.0,
        music_ratio: 0.0,
        sibilance_ratio: 0.0,
        stereo: None,
        silence_runs: Vec::new(),
        chain_version: anvil_core::CHAIN_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_gain_is_identity() {
        let mut buf = AudioBuffer::from_planar(vec![vec![0.1, -0.2, 0.3]], 48_000);
        let before = buf.clone();
        Gain { linear: 1.0 }.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn six_db_roughly_doubles() {
        let mut buf = AudioBuffer::from_planar(vec![vec![0.25]], 48_000);
        Gain::from_db(6.0206).process(&mut buf);
        assert!((buf.channel(0)[0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn processing_is_deterministic() {
        let make = || AudioBuffer::from_planar(vec![vec![0.11, 0.22, 0.33]], 48_000);
        let (mut a, mut b) = (make(), make());
        Gain::from_db(-3.0).process(&mut a);
        Gain::from_db(-3.0).process(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn analyze_buffer_and_master_agree_on_shape() {
        // A synthetic noisy signal exercised end-to-end without touching the filesystem.
        use std::f32::consts::TAU;
        let n = 48_000 * 4;
        let mut seed = 0x9E37u32;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                0.15 * (i as f32 * 190.0 * TAU / 48_000.0).sin() + 0.05 * noise
            })
            .collect();
        let buf = AudioBuffer::from_planar(vec![s.clone(), s], 48_000);

        let report = analyze_buffer(&buf);
        assert_eq!(report.channels, 2);
        assert_eq!(report.chain_version, anvil_core::CHAIN_VERSION);

        let preset = Preset::default();
        let cfg = auto_configure(&report, &preset, Tier::Standard);
        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);
        assert!(outcome.after.true_peak_dbtp <= preset.true_peak_ceiling_dbtp as f64 + 0.01);
    }

    #[test]
    fn serde_json_keys_match_the_contract() {
        // Lock the AnalysisReport top-level key names (contract E).
        let report = empty_report(2, 48_000);
        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
        for key in [
            "integrated_lufs",
            "true_peak_dbtp",
            "loudness_range_lu",
            "short_term_max_lufs",
            "momentary_max_lufs",
            "duration_secs",
            "sample_rate",
            "channels",
            "noise_floor_dbfs",
            "snr_db",
            "clipping_regions",
            "worst_clip_secs",
            "dc_offset",
            "hum",
            "rt60_secs",
            "reverb_bucket",
            "bandwidth_hz",
            "speech_ratio",
            "music_ratio",
            "sibilance_ratio",
            "stereo",
            "silence_runs",
            "chain_version",
        ] {
            assert!(v.get(key).is_some(), "AnalysisReport missing key {key}");
        }
    }
}
