//! Adaptive leveler (03 §4.8) — the module that shapes most of the listening experience.
//!
//! Two speech-gated, music-aware stages:
//!
//! 1. **Slow AGC.** On 10 ms hops it tracks the speech-gated short-term loudness (K-weighted,
//!    BS.1770) over a 3 s window and nudges a broadband gain toward the target, slewed to
//!    ≤ 2 dB/s over a ±`max_gain` range. Non-speech hops hold the last gain (never pump the
//!    noise floor); in music mode the correction eases back toward 0 dB so intentional
//!    musical dynamics survive. It warm-starts from the analysis so it opens at roughly the
//!    right gain instead of ramping up from 0 dB (03: "converges within 2 s").
//! 2. **Fast tamer.** A 2:1 RMS compressor above target + 6 dB (≤ 6 dB reduction, 5 ms
//!    attack / 150 ms release) catches laughs and shouts the slow stage is too sluggish for.
//!
//! `dynamics_preservation` (0 = broadcast-tight, 1 = off; default 0.35) scales how hard both
//! stages push. All math is deterministic (ADR-003): no time, no threads, no entropy.

use anvil_core::HOP_SAMPLES;
use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::biquad::KWeighting;
use crate::Processor;

/// Leveler configuration. The auto-decision fills these from the analysis + preset.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LevelerConfig {
    /// Target speech short-term loudness in LUFS. Chosen so that, post-normalize, the
    /// integrated loudness lands on the preset target.
    pub target_st_lufs: f32,
    /// Maximum broadband gain magnitude in dB (03: ±12 dB).
    pub max_gain_db: f32,
    /// Dynamics preservation, 0..1 (default 0.35). Scales the applied correction.
    pub dynamics_preservation: f32,
    /// Music-majority material: ease gain toward 0 on non-speech and push gentler.
    pub music_mode: bool,
    /// Loudness (LUFS) below which a hop is treated as non-speech (from the analysis noise
    /// floor + a margin). Gain is held across these hops.
    pub noise_gate_lufs: f32,
    /// Warm-start gain in dB from the analysis, so the AGC opens near the right level.
    pub warm_start_db: f32,
}

impl Default for LevelerConfig {
    fn default() -> Self {
        Self {
            target_st_lufs: -18.0,
            max_gain_db: 12.0,
            dynamics_preservation: 0.35,
            music_mode: false,
            noise_gate_lufs: -50.0,
            warm_start_db: 0.0,
        }
    }
}

/// Max slew per second for the slow AGC (03: ≤ 2 dB/s).
const MAX_SLEW_DB_PER_SEC: f32 = 2.0;
/// Short-term loudness window: 3 s of hops (03 §4.8).
const ST_WINDOW_HOPS: usize = 300;
/// BS.1770 loudness offset for a single K-weighted channel.
const LUFS_OFFSET: f32 = -0.691;

/// Adaptive leveler.
#[derive(Debug, Clone)]
pub struct Leveler {
    config: LevelerConfig,
    sample_rate: f32,
}

impl Leveler {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: LevelerConfig) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
        }
    }

    /// The config.
    pub fn config(&self) -> LevelerConfig {
        self.config
    }
}

/// LUFS of a K-weighted mean-square. Returns a very negative sentinel for silence.
#[inline]
fn ms_to_lufs(mean_square: f32) -> f32 {
    if mean_square > 1e-12 {
        LUFS_OFFSET + 10.0 * mean_square.log10()
    } else {
        -120.0
    }
}

impl Processor for Leveler {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        let channels = buffer.channel_count();
        if frames == 0 || channels == 0 {
            return;
        }

        let dp = self.config.dynamics_preservation.clamp(0.0, 1.0);
        let correction_scale = 1.0 - dp; // dp = 1 ⇒ leveler off
        let hop = HOP_SAMPLES;
        let hop_secs = hop as f32 / self.sample_rate;
        let max_slew_per_hop = MAX_SLEW_DB_PER_SEC * hop_secs;
        let max_gain = self.config.max_gain_db;

        // --- Pass A: per-hop K-weighted mean-square of the mono downmix -------------------
        let n_hops = frames.div_ceil(hop);
        let mut hop_ms = vec![0.0f32; n_hops]; // K-weighted mean-square per hop
        {
            let mut kw = KWeighting::default();
            let inv_ch = 1.0 / channels as f32;
            for (h, ms_slot) in hop_ms.iter_mut().enumerate() {
                let start = h * hop;
                let end = (start + hop).min(frames);
                let mut acc = 0.0f32;
                for i in start..end {
                    let mut mono = 0.0f32;
                    for c in 0..channels {
                        mono += buffer.channel(c)[i];
                    }
                    mono *= inv_ch;
                    let k = kw.process(mono);
                    acc += k * k;
                }
                let len = (end - start).max(1) as f32;
                *ms_slot = acc / len;
            }
        }

        // --- Pass B: slow AGC gain per hop ----------------------------------------------
        // Warm-start so we open near the right gain instead of ramping from 0 dB.
        let mut gain_db = (self.config.warm_start_db * correction_scale).clamp(-max_gain, max_gain);
        let mut window_sum = 0.0f32; // sum of speech-gated ms in the trailing 3 s
        let mut window: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
        let mut hop_gain_db = vec![0.0f32; n_hops];

        for h in 0..n_hops {
            let hop_lufs = ms_to_lufs(hop_ms[h]);
            let is_speech = hop_lufs > self.config.noise_gate_lufs;

            // Maintain the speech-gated 3 s short-term loudness window.
            if is_speech {
                window.push_back(hop_ms[h]);
                window_sum += hop_ms[h];
                if window.len() > ST_WINDOW_HOPS {
                    window_sum -= window.pop_front().unwrap();
                }
            }

            let desired_db = if is_speech && !window.is_empty() {
                let st_ms = window_sum / window.len() as f32;
                let st_lufs = ms_to_lufs(st_ms);
                (self.config.target_st_lufs - st_lufs).clamp(-max_gain, max_gain) * correction_scale
            } else if self.config.music_mode {
                0.0 // music/non-speech in music mode: ease correction back toward unity
            } else {
                gain_db // non-speech in speech mode: hold the last gain
            };

            // Slew-rate limit toward the desired gain (≤ 2 dB/s).
            let delta = (desired_db - gain_db).clamp(-max_slew_per_hop, max_slew_per_hop);
            gain_db += delta;
            hop_gain_db[h] = gain_db;
        }

        // --- Pass C: apply the slow gain, per-sample ramped between hop values ----------
        let mut prev_lin = 10f32.powf(hop_gain_db[0] / 20.0);
        #[allow(clippy::needless_range_loop)]
        for h in 0..n_hops {
            let target_lin = 10f32.powf(hop_gain_db[h] / 20.0);
            let start = h * hop;
            let end = (start + hop).min(frames);
            let span = (end - start).max(1) as f32;
            for i in start..end {
                let t = (i - start) as f32 / span;
                let g = prev_lin + (target_lin - prev_lin) * t;
                for c in 0..channels {
                    buffer.channel_mut(c)[i] *= g;
                }
            }
            prev_lin = target_lin;
        }

        // --- Pass D: fast RMS tamer -----------------------------------------------------
        self.fast_tamer(buffer);
    }
}

impl Leveler {
    /// Stage 2: a 2:1 RMS compressor above target + 6 dB, ≤ 6 dB reduction, 5 ms attack /
    /// 150 ms release. Operates on the (already slow-leveled) mono envelope and applies the
    /// resulting gain to every channel so the stereo image is preserved.
    fn fast_tamer(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        let channels = buffer.channel_count();
        let dp = self.config.dynamics_preservation.clamp(0.0, 1.0);
        let scale = 1.0 - dp;
        if scale <= 0.0 {
            return;
        }

        // Threshold amplitude corresponding to target + 6 dB. `target_st_lufs` is loudness,
        // but for a broadband RMS envelope the K-offset cancels well enough here; we treat
        // the target as an RMS reference in dBFS and add the +6 dB headroom (03 §4.8).
        let threshold_db = self.config.target_st_lufs + 6.0;
        let threshold = 10f32.powf(threshold_db / 20.0);
        let ratio = 2.0f32;
        let max_reduction_db = 6.0f32;

        let attack = (-1.0 / (0.005 * self.sample_rate)).exp(); // 5 ms
        let release = (-1.0 / (0.150 * self.sample_rate)).exp(); // 150 ms
        let inv_ch = 1.0 / channels as f32;

        let mut env = 0.0f32; // RMS-ish amplitude envelope
        for i in 0..frames {
            let mut mono = 0.0f32;
            for c in 0..channels {
                mono += buffer.channel(c)[i];
            }
            mono = (mono * inv_ch).abs();

            // Peak-follower envelope with fast attack / slow release.
            let coeff = if mono > env { attack } else { release };
            env = mono + (env - mono) * coeff;

            // Static 2:1 above threshold, capped at 6 dB, softened by dynamics preservation.
            let mut gain = 1.0f32;
            if env > threshold {
                let over_db = 20.0 * (env / threshold).log10();
                let reduction_db = (over_db * (1.0 - 1.0 / ratio)).min(max_reduction_db) * scale;
                gain = 10f32.powf(-reduction_db / 20.0);
            }
            if gain < 1.0 {
                for c in 0..channels {
                    buffer.channel_mut(c)[i] *= gain;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Short-term (3 s) K-weighted loudness sampled every second over the active regions —
    /// the quantity the leveler is supposed to flatten.
    fn st_loudness_series(buf: &AudioBuffer) -> Vec<f32> {
        let ch = buf.channel_count();
        let frames = buf.frames();
        let hop = HOP_SAMPLES;
        let mut kw = KWeighting::default();
        let mut hop_ms = Vec::new();
        for h in 0..frames.div_ceil(hop) {
            let start = h * hop;
            let end = (start + hop).min(frames);
            let mut acc = 0.0;
            for i in start..end {
                let mut m = 0.0;
                for c in 0..ch {
                    m += buf.channel(c)[i];
                }
                let k = kw.process(m / ch as f32);
                acc += k * k;
            }
            hop_ms.push(acc / (end - start).max(1) as f32);
        }
        // 3 s trailing windows, one reading per second, over speech-ish hops only.
        let mut out = Vec::new();
        for center in (300..hop_ms.len()).step_by(100) {
            let w = &hop_ms[center - 300..center];
            let ms: f32 = w.iter().sum::<f32>() / w.len() as f32;
            let lufs = super::ms_to_lufs(ms);
            if lufs > -55.0 {
                out.push(lufs);
            }
        }
        out
    }

    fn variance(x: &[f32]) -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        let mean = x.iter().sum::<f32>() / x.len() as f32;
        x.iter().map(|&v| (v - mean).powi(2)).sum::<f32>() / x.len() as f32
    }

    /// 6 s at −30 dBFS then 6 s at −12 dBFS (a "quiet guest / loud host" step).
    fn level_stepped_speech() -> AudioBuffer {
        let sr = 48_000usize;
        let mut s = Vec::new();
        for i in 0..6 * sr {
            s.push(10f32.powf(-30.0 / 20.0) * (i as f32 * 200.0 * TAU / sr as f32).sin());
        }
        for i in 0..6 * sr {
            s.push(10f32.powf(-12.0 / 20.0) * (i as f32 * 200.0 * TAU / sr as f32).sin());
        }
        AudioBuffer::from_planar(vec![s], sr as u32)
    }

    #[test]
    fn reduces_speech_gated_loudness_variance() {
        let input = level_stepped_speech();
        let before = variance(&st_loudness_series(&input));

        let mut out = input.clone();
        let cfg = LevelerConfig {
            target_st_lufs: -21.0,
            max_gain_db: 12.0,
            dynamics_preservation: 0.1,
            music_mode: false,
            noise_gate_lufs: -55.0,
            warm_start_db: 9.0, // opens near the quiet section's needed gain
        };
        Leveler::new(48_000, cfg).process(&mut out);
        let after = variance(&st_loudness_series(&out));

        assert!(
            after < before * 0.6,
            "leveler should cut ST-loudness variance: before={before}, after={after}"
        );
    }

    #[test]
    fn silence_is_a_safe_noop() {
        let mut buf = AudioBuffer::from_planar(vec![vec![0.0; 48_000]], 48_000);
        Leveler::new(48_000, LevelerConfig::default()).process(&mut buf);
        assert!(buf.channel(0).iter().all(|s| s.is_finite()));
    }

    #[test]
    fn deterministic() {
        let (mut a, mut b) = (level_stepped_speech(), level_stepped_speech());
        Leveler::new(48_000, LevelerConfig::default()).process(&mut a);
        Leveler::new(48_000, LevelerConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }
}
