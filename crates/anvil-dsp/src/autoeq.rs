//! AutoEQ (03 §4.7) — long-term-average-spectrum matching with a handful of bounded bells.
//!
//! We measure the long-term average spectrum (LTAS) of the speech, compare its *shape* (each
//! band relative to a 1 kHz reference, so overall level — the leveler's job — is ignored) to a
//! target speech curve, and correct the gap with up to eight peaking filters. Every band is
//! bounded: ±6 dB, Q ≤ 2 (03 §4.7). Three targets ship — `neutral` (subtle broadcast tilt),
//! `warm` (fuller low-mids, softer top), `presence` (lifted articulation/air). `amount`
//! (0..1) scales every band gain, so the effect ranges from off to the full ±6 dB.
//!
//! Guardrails: we **never boost above the source roll-off** (`AnalysisReport.bandwidth_hz`) —
//! lifting an 8 kHz-limited phone call at 8 kHz just amplifies hiss — and we never boost a
//! band that is essentially empty relative to the loudest band, for the same reason.
//!
//! The LTAS is measured from the buffer handed to `process` (post repair/denoise), on
//! energy-gated frames, with the same 2048-point Hann-windowed FFT the analysis pass uses.
//! Deterministic (ADR-003): fixed windows, sequential FFTs, no entropy.

use anvil_core::HOP_SAMPLES;
use anvil_media::AudioBuffer;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::biquad::Biquad;
use crate::Processor;

/// FFT window (matches the analysis pass — 2048 @ 48 kHz ⇒ 23.4 Hz/bin).
const FFT_SIZE: usize = 2048;
/// Band centres (Hz): eight log-spaced points across the speech-critical range (03 §4.7,
/// "≤ 8 biquads").
const BAND_CENTERS: [f32; 8] = [
    125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 6_000.0, 8_000.0,
];
/// The reference band (1 kHz) that the spectrum shape is measured relative to.
const REF_BAND: usize = 3;
/// Maximum per-band boost/cut (03 §4.7: bounded ±6 dB).
const MAX_GAIN_DB: f32 = 6.0;
/// A band more than this far below the loudest band is treated as empty — never boosted.
const EMPTY_BAND_DB: f32 = 40.0;
/// Gate margin (dB above the 10th-percentile floor) selecting speech-ish frames for the LTAS.
const GATE_MARGIN_DB: f32 = 10.0;

/// Target speech curve. Values are relative-dB shapes referenced to 1 kHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqTarget {
    /// Subtle, natural broadcast tilt.
    Neutral,
    /// Fuller low-mids, gentler high end.
    Warm,
    /// Lifted presence and air for articulation.
    Presence,
}

impl EqTarget {
    /// The target shape (dB relative to 1 kHz) at each [`BAND_CENTERS`] point.
    fn curve(self) -> [f32; 8] {
        match self {
            // 125   250   500   1k    2k    4k    6k    8k
            EqTarget::Neutral => [-1.5, -0.5, 0.0, 0.0, 0.5, 1.0, 0.5, -0.5],
            EqTarget::Warm => [2.0, 2.0, 1.0, 0.0, -0.5, -1.0, -1.5, -2.0],
            EqTarget::Presence => [-2.0, -1.5, -0.5, 0.0, 1.5, 2.5, 2.0, 1.0],
        }
    }
}

/// AutoEQ configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutoEqConfig {
    /// Target speech curve.
    pub target: EqTarget,
    /// Overall strength, 0..1 — scales every band gain.
    pub amount: f32,
    /// Source roll-off (Hz) from the analysis; no band above it is ever boosted (0 = unknown).
    pub bandwidth_hz: f32,
    /// Bell Q (03: ≤ 2).
    pub q: f32,
}

impl Default for AutoEqConfig {
    fn default() -> Self {
        Self {
            target: EqTarget::Neutral,
            amount: 0.5,
            bandwidth_hz: 0.0,
            q: 1.0,
        }
    }
}

/// One fitted band.
///
/// Serde-able because Voice Memory (§4.7, [`crate::speaker`]) persists a speaker's fitted curve
/// between episodes — the storage lane writes these straight into the project file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BandFit {
    /// Centre frequency, Hz.
    pub center_hz: f32,
    /// Applied gain, dB (already clamped to ±6 and scaled by `amount`).
    pub gain_db: f32,
    /// Bell Q.
    pub q: f32,
}

/// AutoEQ processor. (No `Debug` derive: the FFT plan is a `dyn` trait object, matching the
/// analysis pass's `Analyzer`.)
#[derive(Clone)]
pub struct AutoEq {
    config: AutoEqConfig,
    sample_rate: f32,
    fft: Arc<dyn RealToComplex<f32>>,
    hann: Vec<f32>,
}

impl AutoEq {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: AutoEqConfig) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let hann: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        Self {
            config,
            sample_rate: sample_rate as f32,
            fft,
            hann,
        }
    }

    /// The config.
    pub fn config(&self) -> AutoEqConfig {
        self.config
    }

    /// Compute the per-band corrections for this buffer without applying them (also used by
    /// tests). Empty when the buffer is too short to measure or `amount` is 0.
    pub fn fit_bands(&self, buffer: &AudioBuffer) -> Vec<BandFit> {
        let amount = self.config.amount.clamp(0.0, 1.0);
        let frames = buffer.frames();
        let channels = buffer.channel_count();
        if amount <= 0.0 || frames < FFT_SIZE || channels == 0 {
            return Vec::new();
        }

        // Mono downmix.
        let inv_ch = 1.0 / channels as f32;
        let mut mono = vec![0.0f32; frames];
        for c in 0..channels {
            for (i, &s) in buffer.channel(c).iter().enumerate() {
                mono[i] += s * inv_ch;
            }
        }

        let ltas = self.ltas(&mono);
        let bin_hz = self.sample_rate / FFT_SIZE as f32;

        // Integrate the LTAS into the eight bands (geometric-midpoint edges).
        let mut band_db = [0.0f32; 8];
        for (b, slot) in band_db.iter_mut().enumerate() {
            let (lo, hi) = band_edges(b);
            let mut power = 0.0f32;
            for (k, &p) in ltas.iter().enumerate() {
                let f = k as f32 * bin_hz;
                if f >= lo && f < hi {
                    power += p;
                }
            }
            *slot = 10.0 * (power + 1e-12).log10();
        }

        let reference = band_db[REF_BAND];
        let loudest = band_db.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let curve = self.config.target.curve();
        let bw = self.config.bandwidth_hz;

        let mut fits = Vec::new();
        for (b, &center) in BAND_CENTERS.iter().enumerate() {
            let measured_rel = band_db[b] - reference;
            let delta = (curve[b] - measured_rel).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
            let mut gain = delta * amount;

            // Never boost above the source roll-off (03 §4.7).
            if bw > 0.0 && center >= bw {
                gain = gain.min(0.0);
            }
            // Never boost an essentially empty band (would just lift hiss).
            if band_db[b] < loudest - EMPTY_BAND_DB {
                gain = gain.min(0.0);
            }
            if gain.abs() > 0.01 {
                fits.push(BandFit {
                    center_hz: center,
                    gain_db: gain,
                    q: self.config.q.min(2.0),
                });
            }
        }
        fits
    }

    /// Long-term average power spectrum over energy-gated frames.
    fn ltas(&self, mono: &[f32]) -> Vec<f32> {
        let n = mono.len();
        let n_bins = FFT_SIZE / 2 + 1;

        // First pass: per-hop energy to choose a speech-ish gate.
        let n_hops = (n.saturating_sub(FFT_SIZE)) / HOP_SAMPLES + 1;
        let mut hop_db = Vec::with_capacity(n_hops);
        for h in 0..n_hops {
            let start = h * HOP_SAMPLES;
            let seg = &mono[start..start + FFT_SIZE];
            let ms = seg.iter().map(|&s| s * s).sum::<f32>() / FFT_SIZE as f32;
            hop_db.push(if ms > 1e-12 {
                10.0 * ms.log10()
            } else {
                -120.0
            });
        }
        let mut sorted = hop_db.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let floor = if sorted.is_empty() {
            -120.0
        } else {
            sorted[((sorted.len() - 1) as f32 * 0.10).round() as usize]
        };
        let gate = floor + GATE_MARGIN_DB;

        // Second pass: accumulate the windowed power spectrum. Prefer gated (speech-ish) hops;
        // if none clear the gate, fall back to every hop.
        let gated: Vec<usize> = (0..n_hops).filter(|&h| hop_db[h] >= gate).collect();
        let selected: Vec<usize> = if gated.is_empty() {
            (0..n_hops).collect()
        } else {
            gated
        };

        let mut accum = vec![0.0f32; n_bins];
        let mut input = self.fft.make_input_vec();
        let mut spectrum = self.fft.make_output_vec();
        for &h in &selected {
            let start = h * HOP_SAMPLES;
            for (i, slot) in input.iter_mut().enumerate() {
                *slot = mono[start + i] * self.hann[i];
            }
            let _ = self.fft.process(&mut input, &mut spectrum);
            for (a, c) in accum.iter_mut().zip(&spectrum) {
                *a += c.norm_sqr();
            }
        }
        let inv = 1.0 / selected.len().max(1) as f32;
        for a in &mut accum {
            *a *= inv;
        }
        accum
    }
}

/// Geometric-midpoint band edges `[lo, hi)` around [`BAND_CENTERS`]`[b]`.
fn band_edges(b: usize) -> (f32, f32) {
    let c = BAND_CENTERS;
    let lo = if b == 0 {
        c[0] / (c[1] / c[0]).sqrt()
    } else {
        (c[b - 1] * c[b]).sqrt()
    };
    let hi = if b + 1 == c.len() {
        c[b] * (c[b] / c[b - 1]).sqrt()
    } else {
        (c[b] * c[b + 1]).sqrt()
    };
    (lo, hi)
}

impl Processor for AutoEq {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let fits = self.fit_bands(buffer);
        if fits.is_empty() {
            return;
        }
        // One biquad cascade per channel (state is per channel).
        for channel in buffer.planar_mut() {
            let mut bells: Vec<Biquad> = fits
                .iter()
                .map(|f| Biquad::peaking(self.sample_rate, f.center_hz, f.q, f.gain_db))
                .collect();
            for s in channel.iter_mut() {
                let mut x = *s;
                for bell in bells.iter_mut() {
                    x = bell.process(x);
                }
                *s = x;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Broadband speech-ish noise so every band has measurable energy.
    fn broadband(secs: usize) -> AudioBuffer {
        let n = 48_000 * secs;
        let mut seed = 0x2468_1357u32;
        let s: Vec<f32> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.2 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
            })
            .collect();
        AudioBuffer::from_planar(vec![s], 48_000)
    }

    /// A band-limited source: only low-frequency tones, so roll-off is low.
    fn lowpassed(secs: usize) -> AudioBuffer {
        let n = 48_000 * secs;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                0.2 * ((t * 200.0 * TAU).sin() + (t * 600.0 * TAU).sin())
            })
            .collect();
        AudioBuffer::from_planar(vec![s], 48_000)
    }

    #[test]
    fn all_band_gains_within_plus_minus_six_db() {
        let buf = broadband(3);
        let eq = AutoEq::new(
            48_000,
            AutoEqConfig {
                target: EqTarget::Presence,
                amount: 1.0,
                bandwidth_hz: 0.0,
                q: 1.0,
            },
        );
        let fits = eq.fit_bands(&buf);
        assert!(!fits.is_empty(), "broadband source should fit some bands");
        for f in &fits {
            assert!(
                f.gain_db.abs() <= MAX_GAIN_DB + 1e-3,
                "band {} gain {} exceeds ±6 dB",
                f.center_hz,
                f.gain_db
            );
            assert!(f.q <= 2.0 + 1e-6, "Q must stay ≤ 2");
        }
    }

    #[test]
    fn never_boosts_above_rolloff() {
        // A ~700 Hz-limited source with the presence target (which *wants* to boost highs):
        // no band at/above the roll-off may receive a positive gain.
        let buf = lowpassed(3);
        let bandwidth = 700.0;
        let eq = AutoEq::new(
            48_000,
            AutoEqConfig {
                target: EqTarget::Presence,
                amount: 1.0,
                bandwidth_hz: bandwidth,
                q: 1.0,
            },
        );
        for f in eq.fit_bands(&buf) {
            if f.center_hz >= bandwidth {
                assert!(
                    f.gain_db <= 0.0,
                    "band {} above roll-off was boosted (+{} dB)",
                    f.center_hz,
                    f.gain_db
                );
            }
        }
    }

    #[test]
    fn amount_zero_is_a_noop() {
        let buf = broadband(3);
        let mut out = buf.clone();
        let cfg = AutoEqConfig {
            amount: 0.0,
            ..Default::default()
        };
        AutoEq::new(48_000, cfg).process(&mut out);
        assert_eq!(out, buf);
    }

    #[test]
    fn deterministic() {
        let buf = broadband(3);
        let (mut a, mut b) = (buf.clone(), buf.clone());
        let cfg = AutoEqConfig {
            target: EqTarget::Warm,
            amount: 1.0,
            bandwidth_hz: 0.0,
            q: 1.0,
        };
        AutoEq::new(48_000, cfg).process(&mut a);
        AutoEq::new(48_000, cfg).process(&mut b);
        assert_eq!(a, b);
    }
}
