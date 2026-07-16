//! Second-order IIR (biquad) building blocks with hand-derived, standard-cited
//! coefficients. Kept dependency-free so the sample math is fully under our control and
//! bit-deterministic (ADR-003).
//!
//! References:
//! - Robert Bristow-Johnson, "Audio EQ Cookbook" (the high-pass coefficients).
//! - ITU-R BS.1770-4, "Algorithms to measure audio programme loudness and true-peak audio
//!   level" — the two-stage K-weighting pre-filter coefficients (48 kHz).

use std::f32::consts::PI;

/// A transposed-direct-form-II biquad section. One instance per channel; `z1`/`z2` are the
/// two state registers. TDF-II is chosen for its good numerical behavior with f32.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// A biquad from normalized coefficients (a0 already divided out).
    pub fn new(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// 2nd-order high-pass, RBJ Audio EQ Cookbook. `q = 1/√2 ≈ 0.7071` gives a maximally
    /// flat (Butterworth) response — the 03 §4.1 rumble filter.
    pub fn highpass(sample_rate: f32, cutoff_hz: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate;
        let (sin_w0, cos_w0) = (w0.sin(), w0.cos());
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        Self::new(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    /// Peaking (bell) EQ, RBJ Audio EQ Cookbook. `gain_db` is the boost/cut at `center_hz`;
    /// `q` sets the bandwidth. Used for AutoEQ bands (03 §4.7) and, with a large negative
    /// `gain_db`, as the bounded-depth de-hum notch (03 §4.2: gain floor −40 dB, so a peaking
    /// cut rather than a full null keeps speech energy at the harmonic frequency intact).
    pub fn peaking(sample_rate: f32, center_hz: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * center_hz / sample_rate;
        let (sin_w0, cos_w0) = (w0.sin(), w0.cos());
        let alpha = sin_w0 / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;
        Self::new(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    /// Notch (band-reject), RBJ Audio EQ Cookbook. Unity gain everywhere except a null at
    /// `center_hz`, with the notch width set by `q`. The de-hum harmonic filter (03 §4.2):
    /// a true notch returns to unity between harmonics far faster than a deep peaking bell, so
    /// speech either side of a mains harmonic is left alone.
    pub fn notch(sample_rate: f32, center_hz: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * center_hz / sample_rate;
        let (sin_w0, cos_w0) = (w0.sin(), w0.cos());
        let alpha = sin_w0 / (2.0 * q);

        let b0 = 1.0;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        Self::new(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    /// Band-pass (constant 0 dB peak gain), RBJ Audio EQ Cookbook. The de-esser's side-chain
    /// key filter (03 §4.6): isolates the 5–9 kHz sibilant band for level detection.
    pub fn bandpass(sample_rate: f32, center_hz: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * center_hz / sample_rate;
        let (sin_w0, cos_w0) = (w0.sin(), w0.cos());
        let alpha = sin_w0 / (2.0 * q);

        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        Self::new(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    /// BS.1770-4 K-weighting stage 1: the "pre-filter", a high-shelf (~+4 dB) modelling the
    /// acoustic effect of the head. Coefficients are the standard published values at 48 kHz.
    pub fn k_weighting_stage1() -> Self {
        // ITU-R BS.1770-4, Table 1 (48 kHz), a0 normalized to 1.
        Self::new(
            1.535_124_9,
            -2.691_696_2,
            1.198_392_8,
            -1.690_659_3,
            0.732_480_8,
        )
    }

    /// BS.1770-4 K-weighting stage 2: the RLB high-pass (~38 Hz), the second half of the
    /// K-weighting curve. Standard published 48 kHz coefficients.
    pub fn k_weighting_stage2() -> Self {
        // ITU-R BS.1770-4, Table 2 (48 kHz), a0 normalized to 1.
        Self::new(1.0, -2.0, 1.0, -1.990_047_5, 0.990_072_3)
    }

    /// Process one sample (transposed direct form II).
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Clear the state registers.
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// The two-stage BS.1770-4 K-weighting filter (stage 1 → stage 2 in series). Used by the
/// leveler's internal loudness meter so its short-term loudness matches the R128 world.
#[derive(Debug, Clone, Copy)]
pub struct KWeighting {
    stage1: Biquad,
    stage2: Biquad,
}

impl Default for KWeighting {
    fn default() -> Self {
        Self {
            stage1: Biquad::k_weighting_stage1(),
            stage2: Biquad::k_weighting_stage2(),
        }
    }
}

impl KWeighting {
    /// K-weight one sample.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.stage2.process(self.stage1.process(x))
    }

    /// Reset both stages.
    pub fn reset(&mut self) {
        self.stage1.reset();
        self.stage2.reset();
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
    fn peaking_cut_attenuates_at_center_and_passes_far_away() {
        // A −40 dB peaking cut at 60 Hz (Q = 30) knocks a 60 Hz tone far down while a
        // 200 Hz tone passes essentially untouched (the de-hum notch behaviour, 03 §4.2).
        let at_center = tone(60.0, 48_000);
        let far = tone(200.0, 48_000);
        let mut c1 = Biquad::peaking(48_000.0, 60.0, 30.0, -40.0);
        let mut c2 = Biquad::peaking(48_000.0, 60.0, 30.0, -40.0);
        let o_center: Vec<f32> = at_center.iter().map(|&x| c1.process(x)).collect();
        let o_far: Vec<f32> = far.iter().map(|&x| c2.process(x)).collect();

        let a_center = rms(&o_center[24_000..]) / rms(&at_center[24_000..]);
        let a_far = rms(&o_far[24_000..]) / rms(&far[24_000..]);
        assert!(a_center < 0.1, "60 Hz should drop ≥20 dB, got {a_center}");
        assert!(a_far > 0.9, "200 Hz should pass, got {a_far}");
    }

    #[test]
    fn bandpass_passes_center_and_rejects_out_of_band() {
        // A band-pass centred at 6.7 kHz (covering ~5–9 kHz) passes 7 kHz and rejects 500 Hz.
        let center = 6_708.0;
        let q = center / 4_000.0;
        let in_band = tone(7_000.0, 48_000);
        let out_band = tone(500.0, 48_000);
        let mut b1 = Biquad::bandpass(48_000.0, center, q);
        let mut b2 = Biquad::bandpass(48_000.0, center, q);
        let o_in: Vec<f32> = in_band.iter().map(|&x| b1.process(x)).collect();
        let o_out: Vec<f32> = out_band.iter().map(|&x| b2.process(x)).collect();
        assert!(rms(&o_in[24_000..]) > 0.5, "7 kHz should pass the band");
        assert!(rms(&o_out[24_000..]) < 0.1, "500 Hz should be rejected");
    }

    #[test]
    fn highpass_attenuates_below_cutoff_and_passes_above() {
        // 80 Hz Butterworth HPF: a 30 Hz tone is knocked well down, 1 kHz passes ~unity.
        let low = tone(30.0, 48_000);
        let high = tone(1000.0, 48_000);

        let mut hp_low = Biquad::highpass(48_000.0, 80.0, std::f32::consts::FRAC_1_SQRT_2);
        let mut hp_high = Biquad::highpass(48_000.0, 80.0, std::f32::consts::FRAC_1_SQRT_2);

        let out_low: Vec<f32> = low.iter().map(|&x| hp_low.process(x)).collect();
        let out_high: Vec<f32> = high.iter().map(|&x| hp_high.process(x)).collect();

        // Use the settled tail to avoid the transient.
        let atten_low = rms(&out_low[24_000..]) / rms(&low[24_000..]);
        let atten_high = rms(&out_high[24_000..]) / rms(&high[24_000..]);

        assert!(
            atten_low < 0.3,
            "30 Hz should be attenuated, got {atten_low}"
        );
        assert!(
            atten_high > 0.9,
            "1 kHz should pass ~unity, got {atten_high}"
        );
    }
}
