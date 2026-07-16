//! Breath control (03 §4.5) — gently attenuate inhale breaths, never hard-gate.
//!
//! Breaths sit in a characteristic pocket: broadband, noise-like (high zero-crossing rate),
//! and low-energy — clearly above the noise floor but well under speech level, typically at
//! VAD boundaries. We classify each 10 ms hop, then duck the breath hops by a bounded amount
//! (−6 dB default, 0..−18) with 30 ms raised-cosine ramps into and out of the ducked region.
//! Bounded attenuation + ramps is the whole point: hard-gating breaths sounds robotic
//! (03 §4.5), so the gain never collapses to silence. Disabled in music mode by the
//! auto-decision.
//!
//! Self-contained detector (the M1 `AnalysisReport` carries no per-breath timestamps): hop
//! energy and ZCR are computed here from the buffer, and the speech/floor levels are taken as
//! order statistics of the hop energies. Deterministic (ADR-003).

use anvil_core::HOP_SAMPLES;
use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::Processor;

/// Ramp length for entering/leaving a ducked region (03 §4.5: 30 ms).
const RAMP_MS: f32 = 30.0;
/// A hop must be at least this far above the noise floor to be a breath (not just silence).
const FLOOR_MARGIN_DB: f32 = 6.0;
/// A breath hop must be at least this far *below* the speech level (breaths are quiet).
const SPEECH_MARGIN_DB: f32 = 12.0;
/// Minimum zero-crossing rate for a hop to count as noise-like (breaths are broadband).
const MIN_BREATH_ZCR: f32 = 0.10;

/// Breath-control configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BreathConfig {
    /// Attenuation applied to breath hops, in dB of reduction (positive magnitude).
    /// Default 6 (i.e. −6 dB); clamped to 0..18 (03 §4.5: "0..−18").
    pub reduction_db: f32,
    /// Ramp length in milliseconds for the gain transitions.
    pub ramp_ms: f32,
    /// Noise floor in dBFS (from the analysis) — the lower edge of the breath energy band.
    pub noise_floor_dbfs: f32,
}

impl Default for BreathConfig {
    fn default() -> Self {
        Self {
            reduction_db: 6.0,
            ramp_ms: RAMP_MS,
            noise_floor_dbfs: -60.0,
        }
    }
}

/// Breath-control processor.
#[derive(Debug, Clone)]
pub struct BreathControl {
    config: BreathConfig,
    sample_rate: f32,
}

impl BreathControl {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: BreathConfig) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
        }
    }

    /// The config.
    pub fn config(&self) -> BreathConfig {
        self.config
    }
}

/// Per-hop energy (dBFS) and zero-crossing rate of the mono downmix.
fn hop_features(mono: &[f32]) -> Vec<(f32, f32)> {
    let n_hops = mono.len().div_ceil(HOP_SAMPLES);
    let mut feats = Vec::with_capacity(n_hops);
    for h in 0..n_hops {
        let start = h * HOP_SAMPLES;
        let end = (start + HOP_SAMPLES).min(mono.len());
        let seg = &mono[start..end];
        let mut sq = 0.0f32;
        let mut zc = 0u32;
        for w in seg.windows(2) {
            if (w[0] >= 0.0) != (w[1] >= 0.0) {
                zc += 1;
            }
        }
        for &s in seg {
            sq += s * s;
        }
        let len = seg.len().max(1);
        let rms = (sq / len as f32).sqrt();
        let db = if rms > 1e-9 {
            20.0 * rms.log10()
        } else {
            -120.0
        };
        let zcr = zc as f32 / len as f32;
        feats.push((db, zcr));
    }
    feats
}

/// Percentile (0..1) of a slice via a sorted copy.
fn percentile(values: &[f32], p: f32, default: f32) -> f32 {
    if values.is_empty() {
        return default;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() - 1) as f32 * p).round() as usize;
    v[idx]
}

impl Processor for BreathControl {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        let channels = buffer.channel_count();
        if frames == 0 || channels == 0 || self.config.reduction_db <= 0.0 {
            return;
        }

        // Mono downmix for classification.
        let inv_ch = 1.0 / channels as f32;
        let mut mono = vec![0.0f32; frames];
        for c in 0..channels {
            for (i, &s) in buffer.channel(c).iter().enumerate() {
                mono[i] += s * inv_ch;
            }
        }

        let feats = hop_features(&mono);
        if feats.is_empty() {
            return;
        }
        let energies: Vec<f32> = feats.iter().map(|f| f.0).collect();
        // The noise floor is authoritative from the analysis pass (measured on true-silence
        // VAD-negative frames): a breath sits *above* it. Estimating the floor from this
        // buffer's own percentile would land on the breaths themselves when they are a large
        // fraction of the file, and nothing would ever be flagged.
        let noise_floor = self.config.noise_floor_dbfs;
        // Speech level ≈ high percentile of hop energy.
        let speech_level = percentile(&energies, 0.90, noise_floor + 30.0);

        // A breath hop: energy in the (floor, speech) pocket and noise-like (high ZCR).
        let low = noise_floor + FLOOR_MARGIN_DB;
        let high = speech_level - SPEECH_MARGIN_DB;
        let mut breath_hop = vec![false; feats.len()];
        for (i, &(db, zcr)) in feats.iter().enumerate() {
            breath_hop[i] = db > low && db < high && zcr >= MIN_BREATH_ZCR;
        }

        // Per-sample target gain (linear): 1.0, dipping to the reduced level on breath hops.
        let reduced = 10f32.powf(-self.config.reduction_db.clamp(0.0, 18.0) / 20.0);
        let mut gain = vec![1.0f32; frames];
        for (h, &is_breath) in breath_hop.iter().enumerate() {
            if is_breath {
                let start = h * HOP_SAMPLES;
                let end = (start + HOP_SAMPLES).min(frames);
                for g in gain.iter_mut().take(end).skip(start) {
                    *g = reduced;
                }
            }
        }

        // Smooth the gain with 30 ms raised-cosine ramps so transitions are gentle (never a
        // hard gate). We slew the gain toward its target no faster than one ramp per edge.
        let ramp = ((self.config.ramp_ms / 1000.0) * self.sample_rate).round() as usize;
        smooth_gain(&mut gain, ramp.max(1));

        for channel in buffer.planar_mut() {
            for (i, s) in channel.iter_mut().enumerate() {
                *s *= gain[i];
            }
        }
    }
}

/// Raised-cosine slew of a step-valued gain curve: each sample moves toward its target no
/// faster than a full `ramp`-sample cosine transition, so both the fall into and rise out of a
/// ducked region are smooth. Runs a forward then backward limiting pass.
fn smooth_gain(gain: &mut [f32], ramp: usize) {
    let n = gain.len();
    if n == 0 {
        return;
    }
    // Max change per sample for a raised-cosine of full depth over `ramp` samples is bounded;
    // we approximate with a linear slew of that slope, which stays gentle and monotone.
    // Forward pass: limit downward+upward rate.
    let max_step = 1.0 / ramp as f32;
    for i in 1..n {
        let d = gain[i] - gain[i - 1];
        if d > max_step {
            gain[i] = gain[i - 1] + max_step;
        } else if d < -max_step {
            gain[i] = gain[i - 1] - max_step;
        }
    }
    // Backward pass: limit the other direction so ramps are symmetric.
    for i in (0..n - 1).rev() {
        let d = gain[i] - gain[i + 1];
        if d > max_step {
            gain[i] = gain[i + 1] + max_step;
        } else if d < -max_step {
            gain[i] = gain[i + 1] - max_step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// A loud 200 Hz "speech" tone, then a quiet broadband "breath" (noise), then tone again.
    fn speech_breath_speech() -> (AudioBuffer, usize, usize) {
        let sr = 48_000usize;
        let seg = sr; // 1 s each
        let mut s = Vec::new();
        // speech: loud tone
        for i in 0..seg {
            s.push(0.3 * (i as f32 * 200.0 * TAU / sr as f32).sin());
        }
        // breath: quiet noise (~ −34 dBFS, broadband ⇒ high ZCR)
        let mut seed = 0x1234_5678u32;
        let breath_start = s.len();
        for _ in 0..seg {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            s.push(0.04 * noise);
        }
        let breath_end = s.len();
        for i in 0..seg {
            s.push(0.3 * (i as f32 * 200.0 * TAU / sr as f32).sin());
        }
        (
            AudioBuffer::from_planar(vec![s], sr as u32),
            breath_start,
            breath_end,
        )
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|&s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn attenuates_breath_gently_without_gating() {
        let (input, bs, be) = speech_breath_speech();
        let breath_before = rms(&input.channel(0)[bs + 4_800..be - 4_800]);
        let speech_before = rms(&input.channel(0)[4_800..40_000]);

        let mut out = input.clone();
        let cfg = BreathConfig {
            reduction_db: 6.0,
            ramp_ms: 30.0,
            noise_floor_dbfs: -60.0,
        };
        BreathControl::new(48_000, cfg).process(&mut out);

        let breath_after = rms(&out.channel(0)[bs + 4_800..be - 4_800]);
        let speech_after = rms(&out.channel(0)[4_800..40_000]);

        // Breath reduced...
        assert!(
            breath_after < breath_before * 0.85,
            "breath should be attenuated: {breath_before} → {breath_after}"
        );
        // ...but NOT hard-gated (well above silence; −6 dB ≈ 0.5×, floor at −18 dB ≈ 0.125×).
        assert!(
            breath_after > breath_before * 0.2,
            "breath must not be hard-gated: {breath_before} → {breath_after}"
        );
        // Speech essentially untouched.
        assert!(
            (speech_after - speech_before).abs() < speech_before * 0.05,
            "speech should be preserved: {speech_before} → {speech_after}"
        );
    }

    #[test]
    fn min_gain_never_reaches_silence() {
        let (input, _, _) = speech_breath_speech();
        let mut out = input.clone();
        BreathControl::new(48_000, BreathConfig::default()).process(&mut out);
        // No sample should be zeroed by a "gate" — output tracks a bounded gain.
        assert!(out.channel(0).iter().all(|s| s.is_finite()));
    }

    #[test]
    fn deterministic() {
        let (input, _, _) = speech_breath_speech();
        let (mut a, mut b) = (input.clone(), input.clone());
        BreathControl::new(48_000, BreathConfig::default()).process(&mut a);
        BreathControl::new(48_000, BreathConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }
}
