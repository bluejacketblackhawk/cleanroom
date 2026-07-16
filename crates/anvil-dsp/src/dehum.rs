//! De-hum (03 §4.2) — cascaded IIR notches at the detected mains fundamental and its
//! harmonics.
//!
//! Mains hum is a stack of tones at the line frequency (50 Hz in most of the world, 60 Hz in
//! the Americas) and its integer harmonics. We remove it with a cascade of narrow notches —
//! one per harmonic up to ~1 kHz. A true notch returns to unity between harmonics far faster
//! than a deep peaking bell, so speech either side of a harmonic is untouched, while the null
//! is filled back to a bounded −40 dB floor (03 §4.2) by a small dry blend so we never carve
//! an unnaturally infinite hole. The fundamental (50/60) and engagement come from the analysis
//! (`AnalysisReport.hum`); we **never notch blind** — the auto-decision only builds this module
//! when the analyzer confirmed a (preferably stable) tonal peak.
//!
//! Deterministic (ADR-003): fixed coefficients, pure per-sample IIR math, no entropy.

use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::biquad::Biquad;
use crate::Processor;

/// Highest harmonic frequency we bother notching (03 §4.2: "harmonics up to ~1 kHz"). Hum
/// energy above this is negligible and overlaps too much speech to touch safely.
const MAX_HARMONIC_HZ: f32 = 1_000.0;
/// Dry-signal blend that fills the composite null back up to the −40 dB depth floor
/// (03 §4.2: "gain floor −40 dB"; 10^(−40/20) = 0.01).
const FLOOR_BLEND: f32 = 0.01;
/// Notch Q. High enough that each cut is a few Hz wide at the fundamental (60/30 ≈ 2 Hz
/// bandwidth) so speech is untouched, low enough to stay numerically well-behaved in f32.
const NOTCH_Q: f32 = 30.0;

/// De-hum configuration. Produced by the auto-decision from `AnalysisReport.hum`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeHumConfig {
    /// Detected mains fundamental in Hz (≈ 50 or 60).
    pub fundamental_hz: f32,
    /// Removal strength, 0..1. Scales each notch toward the −40 dB depth floor.
    pub strength: f32,
    /// Notch Q (bandwidth control); higher = narrower.
    pub q: f32,
}

impl Default for DeHumConfig {
    fn default() -> Self {
        Self {
            fundamental_hz: 60.0,
            strength: 1.0,
            q: NOTCH_Q,
        }
    }
}

impl DeHumConfig {
    /// Number of harmonics (including the fundamental) that fall at or below [`MAX_HARMONIC_HZ`].
    fn harmonic_count(&self) -> usize {
        if self.fundamental_hz <= 0.0 {
            return 0;
        }
        (MAX_HARMONIC_HZ / self.fundamental_hz).floor() as usize
    }
}

/// De-hum processor: one cascade of harmonic notch biquads per channel.
#[derive(Debug, Clone)]
pub struct DeHum {
    config: DeHumConfig,
    sample_rate: f32,
    /// `notches[channel][harmonic]` — TDF-II state is per channel so channels stay independent.
    notches: Vec<Vec<Biquad>>,
}

impl DeHum {
    /// Build for `channels` channels at `sample_rate` with `config`.
    pub fn new(channels: usize, sample_rate: u32, config: DeHumConfig) -> Self {
        let sample_rate = sample_rate as f32;
        let channels = channels.max(1);
        let notches = (0..channels)
            .map(|_| Self::build_cascade(sample_rate, config))
            .collect();
        Self {
            config,
            sample_rate,
            notches,
        }
    }

    /// One channel's harmonic-notch cascade (true notches; depth is bounded later by the
    /// dry blend in [`Processor::process`]).
    fn build_cascade(sample_rate: f32, config: DeHumConfig) -> Vec<Biquad> {
        let n = config.harmonic_count();
        (1..=n)
            .map(|h| {
                let freq = config.fundamental_hz * h as f32;
                Biquad::notch(sample_rate, freq, config.q)
            })
            .collect()
    }

    /// The config.
    pub fn config(&self) -> DeHumConfig {
        self.config
    }
}

impl Processor for DeHum {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        if self.config.strength <= 0.0 || self.notches.first().map(Vec::is_empty).unwrap_or(true) {
            return;
        }
        // Blend so a full null lands at the −40 dB floor and `strength` scales the effect:
        //   out = dry·(1−w+w·ε) + wet·w·(1−ε),  ε = FLOOR_BLEND, w = strength.
        let w = self.config.strength.clamp(0.0, 1.0);
        let dry_coeff = (1.0 - w) + w * FLOOR_BLEND;
        let wet_coeff = w * (1.0 - FLOOR_BLEND);
        for (ch_idx, channel) in buffer.planar_mut().iter_mut().enumerate() {
            if ch_idx >= self.notches.len() {
                self.notches
                    .push(Self::build_cascade(self.sample_rate, self.config));
            }
            let cascade = &mut self.notches[ch_idx];
            for sample in channel.iter_mut() {
                let dry = *sample;
                let mut wet = dry;
                for notch in cascade.iter_mut() {
                    wet = notch.process(wet);
                }
                *sample = dry * dry_coeff + wet * wet_coeff;
            }
        }
    }

    fn reset(&mut self) {
        for cascade in &mut self.notches {
            for notch in cascade.iter_mut() {
                notch.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * freq * TAU / 48_000.0).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|&s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn notches_60hz_by_at_least_20db_while_passing_200hz() {
        let cfg = DeHumConfig {
            fundamental_hz: 60.0,
            strength: 1.0,
            q: NOTCH_Q,
        };
        let mut hum = AudioBuffer::from_planar(vec![tone(60.0, 48_000)], 48_000);
        let mut speech = AudioBuffer::from_planar(vec![tone(200.0, 48_000)], 48_000);
        let hum_before = rms(&hum.channel(0)[24_000..]);
        let speech_before = rms(&speech.channel(0)[24_000..]);

        DeHum::new(1, 48_000, cfg).process(&mut hum);
        DeHum::new(1, 48_000, cfg).process(&mut speech);

        let hum_atten = rms(&hum.channel(0)[24_000..]) / hum_before;
        let speech_ratio = rms(&speech.channel(0)[24_000..]) / speech_before;
        assert!(
            hum_atten < 0.1,
            "60 Hz hum should drop ≥20 dB, got {hum_atten}"
        );
        assert!(
            speech_ratio > 0.9,
            "200 Hz (between harmonics) should pass, got {speech_ratio}"
        );
    }

    #[test]
    fn attenuates_a_harmonic_180hz() {
        // The 3rd harmonic of 60 Hz should also be notched.
        let cfg = DeHumConfig::default();
        let mut buf = AudioBuffer::from_planar(vec![tone(180.0, 48_000)], 48_000);
        let before = rms(&buf.channel(0)[24_000..]);
        DeHum::new(1, 48_000, cfg).process(&mut buf);
        let after = rms(&buf.channel(0)[24_000..]) / before;
        assert!(after < 0.2, "180 Hz harmonic should be cut, got {after}");
    }

    #[test]
    fn fifty_hz_variant_covers_more_harmonics() {
        let cfg = DeHumConfig {
            fundamental_hz: 50.0,
            ..Default::default()
        };
        // 1000 / 50 = 20 harmonics; 1000 / 60 = 16.
        assert_eq!(cfg.harmonic_count(), 20);
        assert_eq!(DeHumConfig::default().harmonic_count(), 16);
    }

    #[test]
    fn deterministic() {
        let cfg = DeHumConfig::default();
        let make =
            || AudioBuffer::from_planar(vec![tone(60.0, 20_000), tone(60.0, 20_000)], 48_000);
        let (mut a, mut b) = (make(), make());
        DeHum::new(2, 48_000, cfg).process(&mut a);
        DeHum::new(2, 48_000, cfg).process(&mut b);
        assert_eq!(a, b);
    }
}
