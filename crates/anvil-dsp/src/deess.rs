//! De-esser (03 §4.6) — split-band compressor keyed on the 5–9 kHz sibilant band.
//!
//! Harsh "ess/sh" sounds are bursts of energy in the 5–9 kHz band. We watch the ratio of
//! side-chain 5–9 kHz energy to broadband energy and, when it exceeds a threshold, apply a
//! dynamic cut to that band (3:1, 1 ms attack, 60 ms release) as `out = x − (1−gain)·band`,
//! where `band` is a 5–9 kHz band-pass of the signal — so at `gain = 1` the output is `x`
//! exactly and only the sibilant band is pulled down. Keying on the *ratio* rather than
//! absolute level pulls sibilant frames down to a target margin over the speech average
//! without dulling everything else (03 §4.6). The threshold is set from the analysis
//! sibilance statistics.
//!
//! Speech-gated (only ducks when the broadband envelope is above a floor) and 5 ms
//! look-ahead: like the true-peak limiter, the look-ahead is realised as a sliding-min over
//! the computed gain so the duck arrives just before the sibilant onset — no added latency,
//! which keeps the module sample-aligned in the un-compensated M1 chain. Deterministic.

use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::biquad::Biquad;
use crate::Processor;

/// Sibilant band centre — geometric mean of 5 and 9 kHz. Used both as the side-chain key and
/// as the dynamic-cut band.
const KEY_CENTER_HZ: f32 = 6_708.0;
/// Side-chain key band width in Hz (5–9 kHz ⇒ 4 kHz wide).
const KEY_BANDWIDTH_HZ: f32 = 4_000.0;

/// De-esser configuration. `threshold_ratio` comes from the analysis sibilance stats.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeEsserConfig {
    /// Side-chain trigger: duck when (5–9 kHz envelope / broadband envelope) exceeds this.
    /// Set from `AnalysisReport.sibilance_ratio` so only genuinely sibilant frames trip it.
    pub threshold_ratio: f32,
    /// Compression ratio above threshold (03: 3:1).
    pub ratio: f32,
    /// Attack time, ms (03: 1 ms).
    pub attack_ms: f32,
    /// Release time, ms (03: 60 ms).
    pub release_ms: f32,
    /// Look-ahead, ms (03: 5 ms).
    pub lookahead_ms: f32,
    /// Speech gate: only de-ess where the broadband envelope exceeds this dBFS.
    pub gate_dbfs: f32,
}

impl Default for DeEsserConfig {
    fn default() -> Self {
        Self {
            threshold_ratio: 0.2,
            ratio: 3.0,
            attack_ms: 1.0,
            release_ms: 60.0,
            lookahead_ms: 5.0,
            gate_dbfs: -55.0,
        }
    }
}

/// De-esser processor.
#[derive(Debug, Clone)]
pub struct DeEsser {
    config: DeEsserConfig,
    sample_rate: f32,
}

impl DeEsser {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: DeEsserConfig) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
        }
    }

    /// The config.
    pub fn config(&self) -> DeEsserConfig {
        self.config
    }
}

/// Sliding-window minimum over the forward window `[n, n+window)` — the look-ahead attack, so
/// the gain is already pulled down before the sibilant peak (O(n), monotonic deque).
fn sliding_min(values: &[f32], window: usize) -> Vec<f32> {
    use std::collections::VecDeque;
    let n = values.len();
    let mut out = vec![1.0f32; n];
    let mut dq: VecDeque<usize> = VecDeque::new();
    for i in (0..n).rev() {
        while let Some(&back) = dq.back() {
            if values[back] >= values[i] {
                dq.pop_back();
            } else {
                break;
            }
        }
        dq.push_back(i);
        if let Some(&front) = dq.front() {
            if front >= i + window {
                dq.pop_front();
            }
        }
        out[i] = values[*dq.front().unwrap()];
    }
    out
}

impl Processor for DeEsser {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        let channels = buffer.channel_count();
        if frames == 0 || channels == 0 {
            return;
        }

        // Mono side-chain: broadband + band-passed 5–9 kHz key.
        let inv_ch = 1.0 / channels as f32;
        let mut key_bp = Biquad::bandpass(
            self.sample_rate,
            KEY_CENTER_HZ,
            KEY_CENTER_HZ / KEY_BANDWIDTH_HZ,
        );

        let attack = (-1.0 / (self.config.attack_ms / 1000.0 * self.sample_rate)).exp();
        let release = (-1.0 / (self.config.release_ms / 1000.0 * self.sample_rate)).exp();
        let gate_lin = 10f32.powf(self.config.gate_dbfs / 20.0);
        let ratio = self.config.ratio.max(1.0);
        let thr = self.config.threshold_ratio.max(1e-4);

        // Per-sample gain reduction for the high band from the side-chain ratio. We smooth
        // **power** (mean-square), not rectified amplitude: a one-pole on x² converges to the
        // true A²/2 of a sinusoid regardless of its frequency, so the 5–9 kHz key and the
        // broadband reference are compared on an equal footing (a rectified peak-follower
        // under-reads high frequencies and would barely trip).
        let mut broad_ms = 0.0f32;
        let mut key_ms = 0.0f32;
        let mut g_req = vec![1.0f32; frames];
        for (n, g) in g_req.iter_mut().enumerate() {
            let mut mono = 0.0f32;
            for c in 0..channels {
                mono += buffer.channel(c)[n];
            }
            mono *= inv_ch;
            let key = key_bp.process(mono);
            let key_p = key * key;
            let broad_p = mono * mono;

            // Fast-attack / slow-release envelopes on the power of both signals.
            broad_ms = envelope(broad_ms, broad_p, attack, release);
            key_ms = envelope(key_ms, key_p, attack, release);

            let broad_rms = broad_ms.sqrt();
            if broad_rms > gate_lin {
                let ratio_now = (key_ms / broad_ms.max(1e-12)).sqrt();
                if ratio_now > thr {
                    // Excess sibilance, in dB, compressed at `ratio:1`.
                    let over_db = 20.0 * (ratio_now / thr).log10();
                    let reduction_db = over_db * (1.0 - 1.0 / ratio);
                    *g = 10f32.powf(-reduction_db / 20.0);
                }
            }
        }

        // Look-ahead: duck slightly before the onset (no added latency; see module docs).
        let lookahead = ((self.config.lookahead_ms / 1000.0) * self.sample_rate).round() as usize;
        let g_look = sliding_min(&g_req, lookahead.max(1) + 1);

        // Apply as a dynamic band cut: out = x − (1−gain)·bandpass(x). Subtracting a *band-pass*
        // copy (not a high-pass) matters — an RBJ band-pass is in phase (H = 1, real) at its
        // centre, so the sibilant band is attenuated cleanly by (1−gain); a high-pass would
        // phase-rotate the ess and the subtraction would barely reduce its amplitude. Perfect
        // reconstruction when gain = 1, and the low band (band-pass ≈ 0) is untouched.
        for channel in buffer.planar_mut() {
            let mut bp = Biquad::bandpass(
                self.sample_rate,
                KEY_CENTER_HZ,
                KEY_CENTER_HZ / KEY_BANDWIDTH_HZ,
            );
            for (n, s) in channel.iter_mut().enumerate() {
                let band = bp.process(*s);
                *s -= (1.0 - g_look[n]) * band;
            }
        }
    }
}

/// One-pole peak-follower step: fast attack, slow release.
#[inline]
fn envelope(prev: f32, x: f32, attack: f32, release: f32) -> f32 {
    let coeff = if x > prev { attack } else { release };
    x + (prev - x) * coeff
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|&s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// A vowel-ish 300 Hz body with a burst of 7 kHz "ess" energy in the middle.
    fn signal_with_sibilant_burst() -> (AudioBuffer, usize, usize) {
        let sr = 48_000usize;
        let n = sr; // 1 s
        let burst_start = sr / 3;
        let burst_end = 2 * sr / 3;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let body = 0.25 * (t * 300.0 * TAU).sin();
                let ess = if i >= burst_start && i < burst_end {
                    0.25 * (t * 7_000.0 * TAU).sin()
                } else {
                    0.0
                };
                body + ess
            })
            .collect();
        (
            AudioBuffer::from_planar(vec![s], sr as u32),
            burst_start,
            burst_end,
        )
    }

    /// Band-limited RMS of the 5–9 kHz content, to measure the "ess" energy.
    fn sibilant_rms(buf: &AudioBuffer) -> f32 {
        let mut bp = Biquad::bandpass(48_000.0, KEY_CENTER_HZ, KEY_CENTER_HZ / KEY_BANDWIDTH_HZ);
        let filtered: Vec<f32> = buf.channel(0).iter().map(|&x| bp.process(x)).collect();
        rms(&filtered)
    }

    #[test]
    fn reduces_a_7khz_sibilant_burst() {
        let (input, _, _) = signal_with_sibilant_burst();
        let before = sibilant_rms(&input);

        let mut out = input.clone();
        let cfg = DeEsserConfig {
            threshold_ratio: 0.15,
            ..Default::default()
        };
        DeEsser::new(48_000, cfg).process(&mut out);
        let after = sibilant_rms(&out);

        assert!(
            after < before * 0.8,
            "de-esser should cut 5–9 kHz energy: before={before}, after={after}"
        );
    }

    #[test]
    fn low_frequency_body_is_preserved() {
        let (input, _, _) = signal_with_sibilant_burst();
        let mut out = input.clone();
        DeEsser::new(48_000, DeEsserConfig::default()).process(&mut out);

        // The de-esser only touches the high band, so overall energy barely moves.
        let body_in = rms(input.channel(0));
        let body_out = rms(out.channel(0));
        assert!(
            (body_out - body_in).abs() < body_in * 0.25,
            "body should be preserved: {body_in} → {body_out}"
        );
    }

    #[test]
    fn deterministic() {
        let (input, _, _) = signal_with_sibilant_burst();
        let (mut a, mut b) = (input.clone(), input.clone());
        DeEsser::new(48_000, DeEsserConfig::default()).process(&mut a);
        DeEsser::new(48_000, DeEsserConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }
}
