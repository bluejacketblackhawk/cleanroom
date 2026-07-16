//! De-clip (03 §4.3, M4) — flat-top detection → cubic reconstruction → peak-safe gain trim.
//!
//! A digitally clipped waveform is easy to recognize and (within limits) easy to undo: the
//! converter or the encoder ran out of numbers, so the excursion that *should* have arched
//! above full scale is instead a **flat top** — a run of consecutive samples pinned to the
//! same extreme value. We find those runs per channel (channels clip independently), then
//! rebuild each one with a **cubic Hermite** curve through the last clean sample before and
//! the first clean sample after the run, using the slopes measured on that clean audio. For a
//! clipped sinusoid that recovers the missing arch almost exactly — the slope where the
//! waveform entered the ceiling tells you how far above it the peak really was — which is the
//! whole point: it is the *harmonic distortion* the clipping created (03 §4.3) that we are
//! removing, not just the visual flatness.
//!
//! Three guard rails, all from the spec:
//!
//! - **Only clipped regions are touched.** Nothing outside a detected flat top is modified.
//! - **Never on percussion** (03 §4.3): the processor is a no-op on music-majority material
//!   (`music_ratio` from the analysis, checked here as well as in `auto_configure`), and a
//!   percussive-but-unclipped hit cannot produce a flat top at full scale anyway.
//! - **Peak-safe gain trim** (03 §4.3): reconstruction *raises* peaks above 0 dBFS by design,
//!   so a static trim afterwards puts the file back under [`DeClipConfig::target_peak_dbfs`]
//!   and the downstream ceiling can never be blown. Two-pass loudness normalize (§4.9) runs
//!   later, so the trim costs nothing in level.
//!
//! Edge cases (03 §8): a **clipped-throughout** source is bounded by `max_overshoot_db` (we
//! restore an arch, we do not invent 20 dB of peak) and by `max_run_ms` (a flat top longer
//! than that carries no recoverable information and is left alone). **32-bit float input
//! already above 0 dBFS** is handled by measuring the clip level from the data — the flat tops
//! sit at whatever value the source pinned to, above or below 1.0 — and by trimming the peak
//! even when nothing needed reconstruction.
//!
//! Deterministic (ADR-003): sequential float math, no entropy.

use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::Processor;

/// A file that never comes within this fraction of full scale cannot be clipped — bail before
/// detecting anything (−0.175 dBFS).
const MIN_CLIP_PEAK: f32 = 0.98;
/// Music-frame guard (03 §4.3 "never engage on percussion", §2 music-majority = > 60%).
const MUSIC_GUARD_RATIO: f32 = 0.6;

/// De-clip configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeClipConfig {
    /// How far below the observed peak a sample still counts as "pinned to the ceiling", in
    /// dB. Real clipped material is not always bit-identical across a flat top (a resampler
    /// or a lossy codec may have run over it since), so the level test has a little slack.
    pub flat_tolerance_db: f32,
    /// Minimum consecutive at-ceiling samples that make a flat top (03 §1 uses ≥ 3 — the same
    /// definition the analysis counts `clipping_regions` with).
    pub min_flat_run: usize,
    /// Longest flat top we will reconstruct, in ms. Beyond this the original excursion is
    /// unknowable; the run is left flat and only the trim applies (03 §8 clipped-throughout).
    pub max_run_ms: f32,
    /// Ceiling on the restored overshoot above the clip level, in dB.
    pub max_overshoot_db: f32,
    /// Peak-safe trim target in dBFS: after reconstruction the sample peak is brought to (at
    /// most) this, so restored peaks cannot blow the downstream limiter's ceiling.
    pub target_peak_dbfs: f32,
    /// Music fraction from the analysis (03 §1). Above [`MUSIC_GUARD_RATIO`] the processor is
    /// a no-op — de-clip never engages on percussion (03 §4.3).
    pub music_ratio: f32,
}

impl Default for DeClipConfig {
    fn default() -> Self {
        Self {
            flat_tolerance_db: 0.05,
            min_flat_run: 3,
            max_run_ms: 10.0,
            max_overshoot_db: 6.0,
            target_peak_dbfs: -1.0,
            music_ratio: 0.0,
        }
    }
}

/// De-clip processor.
#[derive(Debug, Clone)]
pub struct DeClip {
    config: DeClipConfig,
    sample_rate: f32,
}

impl DeClip {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: DeClipConfig) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
        }
    }

    /// The config.
    pub fn config(&self) -> DeClipConfig {
        self.config
    }
}

/// One detected clipped run.
#[derive(Debug, Clone, Copy)]
struct FlatTop {
    start: usize,
    end: usize,
    sign: f32,
}

/// Flat tops in one channel as `[start, end)` runs of samples pinned at (or within the level
/// tolerance of) `clip_level`, with a consistent sign, of length `min_run..=max_run`. Runs
/// touching a buffer edge are skipped: reconstruction needs a clean anchor *and* a slope on
/// both sides.
fn detect_flat_tops(
    channel: &[f32],
    clip_level: f32,
    max_run: usize,
    min_run: usize,
) -> Vec<FlatTop> {
    let n = channel.len();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < n {
        if channel[i].abs() < clip_level {
            i += 1;
            continue;
        }
        let sign = channel[i].signum();
        let start = i;
        let mut end = i + 1;
        while end < n && channel[end].abs() >= clip_level && channel[end].signum() == sign {
            end += 1;
        }
        let len = end - start;
        // Need two clean samples on the left (anchor + slope) and two on the right.
        let has_anchors = start >= 2 && end + 1 < n;
        if len >= min_run && len <= max_run && has_anchors {
            runs.push(FlatTop { start, end, sign });
        }
        i = end;
    }
    runs
}

/// Cubic Hermite basis evaluated at `t ∈ [0, 1]` for endpoints `p0`, `p1` and tangents `m0`,
/// `m1` (both already scaled to the span, i.e. in units of "per unit t").
fn hermite(p0: f32, p1: f32, m0: f32, m1: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    (2.0 * t3 - 3.0 * t2 + 1.0) * p0
        + (t3 - 2.0 * t2 + t) * m0
        + (-2.0 * t3 + 3.0 * t2) * p1
        + (t3 - t2) * m1
}

impl Processor for DeClip {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        if frames < 8 || buffer.channel_count() == 0 {
            return;
        }
        // Percussion guard (03 §4.3): music-majority material is never de-clipped.
        if self.config.music_ratio > MUSIC_GUARD_RATIO {
            return;
        }

        // The clip level is measured from the audio, not assumed to be 1.0: a 32-bit float
        // source can already sit above 0 dBFS (03 §8), in which case the flat tops are pinned
        // wherever the source pinned them.
        let peak_before = buffer
            .planar()
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0f32, |m, &s| m.max(s.abs()));
        if peak_before < MIN_CLIP_PEAK {
            return; // nowhere near full scale — nothing can be clipped
        }
        let clip_level = peak_before * 10f32.powf(-self.config.flat_tolerance_db / 20.0);
        let overshoot_limit = clip_level * 10f32.powf(self.config.max_overshoot_db / 20.0);
        let max_run = (((self.config.max_run_ms / 1000.0) * self.sample_rate).round() as usize)
            .max(self.config.min_flat_run);

        let mut repaired = 0usize;
        let config = self.config;
        for channel in buffer.planar_mut() {
            // Detect on the pristine channel, then patch — a repaired region must never seed
            // the detection of the next one.
            let flat_tops = detect_flat_tops(channel, clip_level, max_run, config.min_flat_run);

            for top in flat_tops {
                // Anchors: the last clean sample before the run and the first clean one after.
                let left = top.start - 1;
                let right = top.end;
                let span = (right - left) as f32;
                // Slopes from the clean audio on each side, scaled to the span (a Hermite
                // tangent is expressed per unit t, not per sample).
                let m0 = (channel[left] - channel[left - 1]) * span;
                let m1 = (channel[right + 1] - channel[right]) * span;
                let (p0, p1) = (channel[left], channel[right]);

                for (offset, sample) in channel[top.start..top.end].iter_mut().enumerate() {
                    // `left` is `top.start − 1`, so the first repaired sample sits at t = 1/span.
                    let t = (offset + 1) as f32 / span;
                    let v = hermite(p0, p1, m0, m1, t);
                    // A clipped sample's true value was *at least* the clip level and had the
                    // run's sign; bound the restored arch so a pathological slope (or a
                    // clipped-throughout square wave, 03 §8) cannot invent an absurd peak.
                    let mag = if v.is_finite() && v.signum() == top.sign {
                        v.abs().clamp(clip_level, overshoot_limit)
                    } else {
                        clip_level
                    };
                    *sample = top.sign * mag;
                }
                repaired += 1;
            }
        }

        // --- Peak-safe gain trim (03 §4.3) ---------------------------------------------------
        // Only when we actually raised peaks, or when the source was already over 0 dBFS
        // (03 §8, float input) — a clean file must pass through bit-identical.
        if repaired == 0 && peak_before <= 1.0 {
            return;
        }
        let peak_after = buffer
            .planar()
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0f32, |m, &s| m.max(s.abs()));
        let target = 10f32.powf(self.config.target_peak_dbfs / 20.0);
        if peak_after > target && peak_after > 0.0 {
            let gain = target / peak_after;
            for channel in buffer.planar_mut() {
                for s in channel.iter_mut() {
                    *s *= gain;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// A hard-clipped sine: `amp`× a 200 Hz tone, clamped to ±1.
    fn clipped_sine(amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (amp * (i as f32 * 200.0 * TAU / 48_000.0).sin()).clamp(-1.0, 1.0))
            .collect()
    }

    /// Count samples sitting on a genuine flat top: ≥ 3 in a row that are high-level *and
    /// essentially identical to their neighbor*. Proximity to the peak alone is not a clipping
    /// test — the crest of any clean low-frequency sine is flat to within a hair for several
    /// samples. It is the *pinning* (successive samples that do not move) that is the clip.
    fn flat_top_samples(x: &[f32]) -> usize {
        let peak = x.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if peak <= 0.0 {
            return 0;
        }
        let level = 0.5 * peak;
        let mut total = 0;
        let mut run = 0;
        for i in 1..x.len() {
            if x[i].abs() >= level && (x[i] - x[i - 1]).abs() <= 1e-5 {
                run += 1;
            } else {
                if run >= 3 {
                    total += run;
                }
                run = 0;
            }
        }
        if run >= 3 {
            total += run;
        }
        total
    }

    /// Total harmonic distortion of a `f0` tone: √(Σ harmonics²)/fundamental, measured with a
    /// rectangular-window DFT over a whole number of periods (no leakage, so the number is
    /// honest). Gain-invariant, which is what we need across the peak-safe trim.
    fn thd(x: &[f32], f0: f32, sample_rate: f32) -> f32 {
        let period = (sample_rate / f0).round() as usize;
        let periods = x.len() / period;
        let n = periods * period;
        let bin = |k: usize| -> f32 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &s) in x[..n].iter().enumerate() {
                let ph = TAU as f64 * k as f64 * i as f64 / n as f64;
                re += s as f64 * ph.cos();
                im -= s as f64 * ph.sin();
            }
            ((re * re + im * im).sqrt() / n as f64) as f32
        };
        let fund = bin(periods).max(1e-12);
        let harmonics: f32 = (2..=9)
            .map(|h| {
                let v = bin(periods * h);
                v * v
            })
            .sum();
        harmonics.sqrt() / fund
    }

    #[test]
    fn hard_clipped_sine_is_restored() {
        let n = 48_000; // exactly 200 periods of 200 Hz — clean DFT bins
        let clipped = clipped_sine(1.5, n);
        let flat_before = flat_top_samples(&clipped);
        let thd_before = thd(&clipped, 200.0, 48_000.0);

        let mut buf = AudioBuffer::from_planar(vec![clipped], 48_000);
        DeClip::new(48_000, DeClipConfig::default()).process(&mut buf);
        let out = buf.channel(0);

        let flat_after = flat_top_samples(out);
        let thd_after = thd(out, 200.0, 48_000.0);
        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));

        assert!(
            flat_before > 1_000,
            "fixture should be heavily clipped, got {flat_before} flat samples"
        );
        assert!(
            flat_after * 20 < flat_before,
            "flat tops should be gone: {flat_before} → {flat_after}"
        );
        assert!(
            thd_after < thd_before * 0.5,
            "clipping distortion should be halved at worst: THD {thd_before:.4} → {thd_after:.4}"
        );
        // Peak-safe: the restored arch is trimmed back under the target (−1 dBFS).
        let target = 10f32.powf(-1.0 / 20.0);
        assert!(
            peak <= target + 1e-4,
            "restored peak {peak} must sit under the {target} trim target"
        );
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn does_not_fire_on_clean_percussive_music() {
        // Hot but unclipped: percussive hits (fast attack, noisy decay) over a bass tone,
        // peaking just under full scale. No flat top ⇒ no reconstruction, no trim.
        let n = 48_000;
        let mut seed = 0xBEEF_0001u32;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let t = i as f32 / 48_000.0;
                let beat = (t * 4.0).fract(); // 4 hits/sec
                let hit = (-40.0 * beat).exp();
                0.75 * hit * noise * 2.0 + 0.2 * (t * 80.0 * TAU).sin()
            })
            .collect();
        let peak = s.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(peak < 1.0, "fixture must not be clipped (peak {peak})");

        let mut buf = AudioBuffer::from_planar(vec![s.clone()], 48_000);
        DeClip::new(48_000, DeClipConfig::default()).process(&mut buf);
        assert_eq!(buf.channel(0), &s[..], "clean percussion must pass through");
    }

    #[test]
    fn music_guard_blocks_even_a_clipped_buffer() {
        let clipped = clipped_sine(1.5, 48_000);
        let mut buf = AudioBuffer::from_planar(vec![clipped.clone()], 48_000);
        DeClip::new(
            48_000,
            DeClipConfig {
                music_ratio: 0.8, // music-majority ⇒ never engage (03 §4.3)
                ..DeClipConfig::default()
            },
        )
        .process(&mut buf);
        assert_eq!(buf.channel(0), &clipped[..], "music guard must be absolute");
    }

    #[test]
    fn float_source_above_full_scale_is_trimmed_not_mangled() {
        // 03 §8: 32-bit float input already > 0 dBFS. Nothing is clipped (the waveform is
        // intact), so there is nothing to reconstruct — but the peak must come down.
        let n = 48_000;
        let hot: Vec<f32> = (0..n)
            .map(|i| 1.4 * (i as f32 * 200.0 * TAU / 48_000.0).sin())
            .collect();
        let mut buf = AudioBuffer::from_planar(vec![hot.clone()], 48_000);
        DeClip::new(48_000, DeClipConfig::default()).process(&mut buf);
        let out = buf.channel(0);
        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        let target = 10f32.powf(-1.0 / 20.0);
        assert!(peak <= target + 1e-4, "over-0 dBFS peak {peak} not trimmed");
        // Waveform shape preserved (pure gain): THD must stay negligible.
        assert!(thd(out, 200.0, 48_000.0) < 0.01);
    }

    #[test]
    fn clipped_throughout_stays_finite_and_bounded() {
        // 03 §8: a source clipped into a near-square wave. We must not explode.
        let square = clipped_sine(8.0, 48_000);
        let mut buf = AudioBuffer::from_planar(vec![square], 48_000);
        DeClip::new(48_000, DeClipConfig::default()).process(&mut buf);
        let out = buf.channel(0);
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            peak <= 10f32.powf(-1.0 / 20.0) + 1e-4,
            "peak {peak} escaped the trim"
        );
    }

    #[test]
    fn deterministic() {
        let s = clipped_sine(1.8, 24_000);
        let make = || AudioBuffer::from_planar(vec![s.clone(), s.clone()], 48_000);
        let (mut a, mut b) = (make(), make());
        DeClip::new(48_000, DeClipConfig::default()).process(&mut a);
        DeClip::new(48_000, DeClipConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn only_clipped_regions_are_touched() {
        let n = 24_000;
        let clipped = clipped_sine(1.5, n);
        let mut buf = AudioBuffer::from_planar(vec![clipped.clone()], 48_000);
        DeClip::new(
            48_000,
            DeClipConfig {
                target_peak_dbfs: 6.0, // disable the trim so we compare like for like
                ..DeClipConfig::default()
            },
        )
        .process(&mut buf);
        let out = buf.channel(0);
        let level = 0.98f32;
        for (i, (&a, &b)) in out.iter().zip(&clipped).enumerate() {
            if b.abs() < level {
                assert_eq!(a, b, "sample {i} outside a flat top was modified");
            }
        }
    }
}
