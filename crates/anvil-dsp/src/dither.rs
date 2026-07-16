//! TPDF dither at final bit-depth reduction (03 §4.11).
//!
//! When the master is rendered to a fixed-point target (24→16), quantization error
//! correlates with the signal and produces audible distortion. Triangular-PDF dither
//! decorrelates it at the cost of a benign noise floor. Off for float/lossy outputs.
//!
//! Determinism (ADR-003): the *only* entropy in the whole engine is this dither, and it is
//! seeded from a hash of the audio content — so a re-render of the same audio produces the
//! same dither noise, hence bit-identical output.

use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::hash::hash_buffer;
use crate::Processor;

/// Dither configuration. The default (`target_bits = None`) is a no-op, matching the M1
/// `master()` API which returns a float `AudioBuffer`; the 16-bit path is exercised by the
/// encoder (M2) and the unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DitherConfig {
    /// Target bit depth. `None` (or 32) means a float output → dither is a no-op.
    pub target_bits: Option<u16>,
}

impl DitherConfig {
    /// Whether dither actually engages (a real fixed-point reduction to ≤ 24 bits).
    pub fn engaged(&self) -> bool {
        matches!(self.target_bits, Some(bits) if bits < 32)
    }
}

/// SplitMix64 — a tiny, fast, well-distributed PRNG. Deterministic given its seed, which is
/// all we need: reproducible dither, not cryptographic randomness.
struct SplitMix64(u64);

impl SplitMix64 {
    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A float in `[0, 1)`.
    #[inline]
    fn next_unit(&mut self) -> f32 {
        // Top 24 bits → [0,1) with full f32 mantissa precision.
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }
}

/// TPDF dither processor.
#[derive(Debug, Clone, Copy)]
pub struct Dither {
    config: DitherConfig,
}

impl Dither {
    /// Build with `config`.
    pub fn new(config: DitherConfig) -> Self {
        Self { config }
    }

    /// The config.
    pub fn config(&self) -> DitherConfig {
        self.config
    }
}

impl Processor for Dither {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let Some(bits) = self.config.target_bits else {
            return;
        };
        if bits >= 32 || buffer.is_empty() {
            return; // float target: dither off (03 §4.11).
        }

        // 1 LSB of the target grid, in the [-1, 1] full-scale domain.
        let lsb = 1.0f32 / (1u32 << (bits - 1)) as f32;

        // Seed from the content hash so the noise is reproducible per-content (ADR-003).
        let seed = hash_buffer(buffer);
        let mut rng = SplitMix64(seed);

        for channel in buffer.planar_mut() {
            for sample in channel.iter_mut() {
                // TPDF over (−1 LSB, +1 LSB): the difference of two independent uniforms.
                let tpdf = (rng.next_unit() - rng.next_unit()) * lsb;
                let dithered = *sample + tpdf;
                // Quantize to the target grid so the value is exactly representable at `bits`.
                *sample = (dithered / lsb).round() * lsb;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone() -> AudioBuffer {
        let s: Vec<f32> = (0..4_800)
            .map(|i| 0.2 * (i as f32 * 440.0 * TAU / 48_000.0).sin())
            .collect();
        AudioBuffer::from_planar(vec![s], 48_000)
    }

    #[test]
    fn float_target_is_a_noop() {
        let mut a = tone();
        let before = a.clone();
        Dither::new(DitherConfig { target_bits: None }).process(&mut a);
        assert_eq!(a, before);
    }

    #[test]
    fn dither_is_deterministic_across_renders() {
        let (mut a, mut b) = (tone(), tone());
        Dither::new(DitherConfig {
            target_bits: Some(16),
        })
        .process(&mut a);
        Dither::new(DitherConfig {
            target_bits: Some(16),
        })
        .process(&mut b);
        assert_eq!(a, b, "same content ⇒ same dithered output");
    }

    #[test]
    fn output_lands_on_the_16bit_grid() {
        let mut a = tone();
        Dither::new(DitherConfig {
            target_bits: Some(16),
        })
        .process(&mut a);
        let lsb = 1.0f32 / (1u32 << 15) as f32;
        for &s in a.channel(0) {
            let steps = s / lsb;
            assert!((steps - steps.round()).abs() < 1e-3, "off-grid sample {s}");
        }
    }
}
