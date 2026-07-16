//! The denoise seam the mastering chain plugs into (03 §4.4).
//!
//! [`Denoiser`] is a thin dispatcher over the three tiers. The chain builds one with
//! [`Denoiser::new`] (Standard) or [`Denoiser::try_with_tier`] and calls [`Denoiser::process`]
//! on the whole buffer; nothing else about the chain changes when a tier's model changes.
//!
//! | Tier | Model | Control |
//! |---|---|---|
//! | Fast | RNNoise (`nnnoiseless`, weights compiled in) | wet/dry mix (the only knob it has) |
//! | **Standard (default)** | **DeepFilterNet3** (ONNX, baked in) | **native `atten_lim_db`, 6..100 dB** |
//! | Studio | heavy enhancer (ONNX, models dir) | native attenuation, GPU-preferred |
//!
//! # The bug this file used to have
//!
//! `strength` was implemented as a wet/dry blend. At the strength the auto-decision picks for
//! a noisy file (0.62) that leaves **38% of the raw noisy signal** in the output by
//! construction, and the chain's two-pass loudness normalisation then lifts the residue by
//! +10 dB. DFN3 and the Studio model both have a *native* attenuation control, so `strength`
//! now drives that instead: 0..1 maps onto a 6..100 dB attenuation limit (spec §4.4). Only the
//! Fast tier still uses a blend, because RNNoise genuinely has nothing else — and Fast is not
//! the default.

use anvil_media::AudioBuffer;

use crate::dfn3::{Dfn3, Dfn3Params, MAX_ATTEN_DB, MIN_ATTEN_DB};
use crate::rnnoise::RnNoise;
use crate::studio::Studio;
use crate::{AiError, DenoiseTier};

/// Denoiser configuration.
///
/// The two fields are the chain's existing knobs and are unchanged — what changed is what
/// `strength` *does* (see [`DenoiseConfig::max_attenuation_db`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DenoiseConfig {
    /// Repair strength, 0..1 — the single user-facing knob (03 §4.4). Maps linearly onto the
    /// model's native maximum-attenuation window, 6..100 dB. It is **not** a wet/dry mix.
    pub strength: f32,
    /// Music-majority file (auto-decision §2): the attenuation limit is halved and clamped so
    /// intros and beds survive ("music frames get <= 50% of speech-frame attenuation").
    pub music_aware: bool,
}

impl DenoiseConfig {
    /// Hard ceiling on the attenuation limit for music-majority material, dB. A 12 dB limit
    /// keeps ~25% of the original spectrum, which is where a musical bed still breathes.
    pub const MUSIC_MAX_ATTEN_DB: f32 = 12.0;

    /// The model's maximum noise attenuation at this strength, in dB — the actual control now,
    /// not a reported intent. Spec §4.4: "6..100 mapped from strength".
    pub fn max_attenuation_db(&self) -> f32 {
        let s = self.strength.clamp(0.0, 1.0);
        let db = MIN_ATTEN_DB + s * (MAX_ATTEN_DB - MIN_ATTEN_DB);
        if self.music_aware {
            (db * 0.5).min(Self::MUSIC_MAX_ATTEN_DB)
        } else {
            db
        }
    }

    /// Fast-tier only: RNNoise has no attenuation control, so `strength` degrades to a wet/dry
    /// mix there. Derived from [`Self::max_attenuation_db`] so both tiers agree on what a given
    /// strength is *meant* to attenuate: `wet = 1 - 10^(-atten_db/20)`.
    pub fn wet_mix(&self) -> f32 {
        let db = self.max_attenuation_db();
        (1.0 - 10f32.powf(-db / 20.0)).clamp(0.0, 1.0)
    }

    fn dfn3_params(&self) -> Dfn3Params {
        Dfn3Params {
            atten_lim_db: self.max_attenuation_db(),
            // Off for Standard: 0 is the upstream default and what the reference DNSMOS numbers
            // were measured with. The Studio tier turns it on.
            post_filter_beta: 0.0,
            // Standard leans on DFN3's own mild dereverb; the explicit stage is Studio's.
            dereverb: 0.0,
        }
    }
}

impl Default for DenoiseConfig {
    fn default() -> Self {
        Self {
            strength: 0.6,
            music_aware: false,
        }
    }
}

enum Engine {
    Fast(RnNoise),
    Standard(Box<Dfn3>),
    Studio(Box<Studio>),
}

/// A 48 kHz denoiser. Build one per track; [`Denoiser::process`] takes the whole buffer.
pub struct Denoiser {
    config: DenoiseConfig,
    tier: DenoiseTier,
    engine: Engine,
}

impl Denoiser {
    /// Build the **Standard** denoiser (DeepFilterNet3) — the one-click default.
    ///
    /// Infallible, so the chain's existing call site is unchanged. If DFN3 cannot be built
    /// (which would mean a broken ONNX Runtime — the model itself is compiled in) we log loudly
    /// and drop to RNNoise rather than fail a render; [`Denoiser::tier`] then reports Fast so
    /// the Health Card can say so.
    pub fn new(channels: usize, config: DenoiseConfig) -> Self {
        match Self::try_with_tier(channels, config, DenoiseTier::Standard) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "DeepFilterNet3 failed to initialise; falling back to RNNoise (Fast). \
                     Denoise quality will be materially worse."
                );
                Self {
                    config,
                    tier: DenoiseTier::Fast,
                    engine: Engine::Fast(RnNoise::new(channels)),
                }
            }
        }
    }

    /// Build a denoiser for an explicit tier.
    pub fn try_with_tier(
        channels: usize,
        config: DenoiseConfig,
        tier: DenoiseTier,
    ) -> Result<Self, AiError> {
        let engine = match tier {
            DenoiseTier::Fast => Engine::Fast(RnNoise::new(channels)),
            DenoiseTier::Standard => Engine::Standard(Box::new(Dfn3::new(config.dfn3_params())?)),
            DenoiseTier::Studio => Engine::Studio(Box::new(Studio::new(config)?)),
        };
        Ok(Self {
            config,
            tier,
            engine,
        })
    }

    /// The active configuration.
    pub fn config(&self) -> DenoiseConfig {
        self.config
    }

    /// The tier actually running (not necessarily the one asked for, if a fallback kicked in).
    pub fn tier(&self) -> DenoiseTier {
        self.tier
    }

    /// The device the tier ended up on (Studio may downgrade GPU -> CPU on a driver failure).
    pub fn device(&self) -> crate::Device {
        match &self.engine {
            Engine::Studio(s) => s.device(),
            _ => crate::Device::Cpu,
        }
    }

    /// Override the post-filter beta and/or the dereverb amount on an already-built denoiser.
    ///
    /// Escape hatch for the eval harness (DNSMOS sweeps) and for a future "advanced" panel. The
    /// auto-decision never calls this; `strength` is the product's knob.
    pub fn tune(&mut self, post_filter_beta: Option<f32>, dereverb: Option<f32>) {
        match &mut self.engine {
            Engine::Standard(d) => {
                let mut p = d.params();
                if let Some(b) = post_filter_beta {
                    p.post_filter_beta = b;
                }
                if let Some(r) = dereverb {
                    p.dereverb = r;
                }
                d.set_params(p);
            }
            Engine::Studio(s) => s.tune(post_filter_beta, dereverb),
            Engine::Fast(_) => {} // RNNoise has neither knob
        }
    }

    /// Reset every channel's state to a clean start (re-render determinism).
    pub fn reset(&mut self) {
        match &mut self.engine {
            Engine::Fast(r) => r.reset(),
            // DFN3 and Studio build fresh STFT state per call, so they are already stateless
            // across `process` calls — a re-render cannot inherit anything.
            Engine::Standard(_) | Engine::Studio(_) => {}
        }
    }

    /// Extra output latency in samples. Every tier is length-preserving and sample-aligned (the
    /// STFT's 480-sample OLA latency is compensated inside), so the chain owes nothing.
    pub fn latency_samples(&self) -> usize {
        0
    }

    /// Denoise `buffer` in place. Errors are logged and the audio is left untouched — a failed
    /// model must never take the render down.
    pub fn process(&mut self, buffer: &mut AudioBuffer) {
        if let Err(e) = self.try_process(buffer) {
            tracing::error!(error = %e, "denoise failed; leaving the audio untouched");
        }
    }

    /// Denoise `buffer` in place, surfacing model errors.
    pub fn try_process(&mut self, buffer: &mut AudioBuffer) -> Result<(), AiError> {
        if buffer.is_empty() {
            return Ok(());
        }
        match &mut self.engine {
            Engine::Fast(r) => {
                let wet = self.config.wet_mix();
                if wet <= 0.0 {
                    return Ok(());
                }
                for (i, channel) in buffer.planar_mut().iter_mut().enumerate() {
                    r.process_channel(i, channel, wet);
                }
            }
            Engine::Standard(d) => {
                for channel in buffer.planar_mut().iter_mut() {
                    d.process_channel(channel)?;
                }
            }
            Engine::Studio(s) => {
                for channel in buffer.planar_mut().iter_mut() {
                    s.process_channel(channel)?;
                }
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("config", &self.config)
            .field("tier", &self.tier)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|&s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// 1 s of tone + deterministic LCG noise (no `rand`, so the test is reproducible).
    fn tone_plus_noise() -> Vec<f32> {
        let n = 48_000;
        let mut seed = 0x1234_5678u32;
        (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let tone = (i as f32 * 220.0 * TAU / 48_000.0).sin();
                0.3 * tone + 0.15 * noise
            })
            .collect()
    }

    /// The regression that started all this: at the strength the auto-decision picks for a
    /// noisy file, `strength` must NOT pass a fraction of the noisy signal straight through.
    #[test]
    fn strength_drives_attenuation_not_a_blend() {
        let cfg = DenoiseConfig {
            strength: 0.62,
            music_aware: false,
        };
        // 0.62 -> 64.3 dB of allowed attenuation ...
        assert!((cfg.max_attenuation_db() - 64.28).abs() < 0.1);
        // ... which is ~0.06% of the noisy signal mixed back, not 38% of it.
        assert!(
            cfg.wet_mix() > 0.999,
            "strength 0.62 must be near-full suppression, got wet={}",
            cfg.wet_mix()
        );
    }

    #[test]
    fn strength_maps_onto_the_spec_window() {
        let at = |s: f32| {
            DenoiseConfig {
                strength: s,
                music_aware: false,
            }
            .max_attenuation_db()
        };
        assert!((at(0.0) - MIN_ATTEN_DB).abs() < 1e-4);
        assert!((at(1.0) - MAX_ATTEN_DB).abs() < 1e-4);
        assert!(at(0.5) > at(0.2));
    }

    #[test]
    fn music_aware_caps_the_attenuation() {
        let cfg = DenoiseConfig {
            strength: 1.0,
            music_aware: true,
        };
        assert_eq!(
            cfg.max_attenuation_db(),
            DenoiseConfig::MUSIC_MAX_ATTEN_DB,
            "music-majority material must stay gentle"
        );
        assert!(cfg.wet_mix() < 0.76); // ~25% of the original spectrum survives
    }

    #[test]
    fn standard_tier_reduces_the_noise_floor_in_gaps() {
        // Tone for the first half, noise only for the second: the denoiser should crush the
        // noise-only tail.
        let mut samples = tone_plus_noise();
        for (i, s) in samples.iter_mut().enumerate().skip(24_000) {
            let mut seed = (i as u32).wrapping_mul(2_654_435_761);
            seed ^= seed >> 15;
            *s = 0.12 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
        }
        let tail_before = rms(&samples[36_000..]);

        let mut buf = AudioBuffer::from_planar(vec![samples], 48_000);
        let mut d = Denoiser::new(
            1,
            DenoiseConfig {
                strength: 1.0,
                music_aware: false,
            },
        );
        assert_eq!(d.tier(), DenoiseTier::Standard, "Standard must be DFN3");
        d.try_process(&mut buf).unwrap();
        let tail_after = rms(&buf.channel(0)[36_000..]);
        assert!(
            tail_after < tail_before * 0.25,
            "DFN3 should crush a noise-only tail: before={tail_before}, after={tail_after}"
        );
    }

    #[test]
    fn fast_tier_still_works() {
        let mut buf = AudioBuffer::from_planar(vec![tone_plus_noise()], 48_000);
        let mut d =
            Denoiser::try_with_tier(1, DenoiseConfig::default(), DenoiseTier::Fast).unwrap();
        assert_eq!(d.tier(), DenoiseTier::Fast);
        d.try_process(&mut buf).unwrap();
        assert!(buf.channel(0).iter().all(|s| s.is_finite()));
    }

    #[test]
    fn denoise_is_deterministic() {
        let make = || AudioBuffer::from_planar(vec![tone_plus_noise()], 48_000);
        let (mut a, mut b) = (make(), make());
        Denoiser::new(1, DenoiseConfig::default())
            .try_process(&mut a)
            .unwrap();
        Denoiser::new(1, DenoiseConfig::default())
            .try_process(&mut b)
            .unwrap();
        assert_eq!(a, b, "a re-render must be bit-identical");
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let mut buf = AudioBuffer::from_planar(vec![vec![0.0; 1000]], 48_000);
        Denoiser::new(1, DenoiseConfig::default())
            .try_process(&mut buf)
            .unwrap();
        assert!(buf.channel(0).iter().all(|s| s.abs() < 1e-6));
        assert!(buf.channel(0).iter().all(|s| s.is_finite()));
    }

    #[test]
    fn stereo_channels_are_processed_independently() {
        let left = tone_plus_noise();
        let right: Vec<f32> = left.iter().map(|s| s * 0.5).collect();
        let mut buf = AudioBuffer::from_planar(vec![left, right], 48_000);
        Denoiser::new(2, DenoiseConfig::default())
            .try_process(&mut buf)
            .unwrap();
        assert_eq!(buf.channel(0).len(), 48_000);
        assert_eq!(buf.channel(1).len(), 48_000);
    }
}
