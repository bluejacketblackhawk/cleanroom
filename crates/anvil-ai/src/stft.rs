//! The DeepFilterNet analysis/synthesis pair and its ERB filterbank.
//!
//! This is a faithful port of `DFState` from the DeepFilterNet project (`libDF/src/lib.rs`,
//! MIT/Apache-2.0). It has to be faithful: DFN3 was *trained* on exactly these features, so
//! any drift in the window, the FFT scaling, the ERB band edges or the running-mean
//! normalisation feeds the network something it has never seen and the output degrades
//! silently. Every constant below is therefore quoted from upstream rather than derived.
//!
//! Layout at 48 kHz (DFN3 `config.ini` `[df]`): `fft_size = 960` (20 ms), `hop_size = 480`
//! (10 ms), `nb_erb = 32`, `nb_df = 96`, `min_nb_erb_freqs = 2`, `norm_tau = 1`.
//!
//! The analysis/synthesis OLA pair has an inherent latency of `fft_size - hop_size` = 480
//! samples: output frame `k` reconstructs input samples `[(k-1)·hop, k·hop)`. [`Stft::run`]
//! compensates for that by running one flush frame and trimming, so the module is
//! length-preserving and sample-aligned (checked by `stft_istft_roundtrip_is_identity`).

use num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Sample rate the DFN3 weights were trained at.
pub const SR: usize = 48_000;
/// FFT / window size (20 ms).
pub const FFT_SIZE: usize = 960;
/// Hop / frame size (10 ms). Also [`anvil_core::HOP_SAMPLES`].
pub const HOP_SIZE: usize = 480;
/// `fft_size / 2 + 1`.
pub const FREQ_SIZE: usize = FFT_SIZE / 2 + 1; // 481
/// Number of ERB bands in `feat_erb`.
pub const NB_ERB: usize = 32;
/// Number of complex bins fed to the DF (deep filtering) stage.
pub const NB_DF: usize = 96;
/// Minimum frequency bins per ERB band.
pub const MIN_NB_ERB_FREQS: usize = 2;
/// Exponential-normalisation time constant, seconds (`norm_tau` in `config.ini`).
pub const NORM_TAU: f32 = 1.0;

/// Running-mean init for the ERB feature, in dB (upstream `MEAN_NORM_INIT`).
const MEAN_NORM_INIT: [f32; 2] = [-60., -90.];
/// Running-mean init for the unit-norm complex feature (upstream `UNIT_NORM_INIT`).
const UNIT_NORM_INIT: [f32; 2] = [0.001, 0.0001];

fn freq2erb(freq_hz: f32) -> f32 {
    9.265 * (freq_hz / (24.7 * 9.265)).ln_1p()
}

fn erb2freq(n_erb: f32) -> f32 {
    24.7 * 9.265 * ((n_erb / 9.265).exp() - 1.)
}

/// Width (in FFT bins) of each ERB band. Sums to [`FREQ_SIZE`].
pub fn erb_widths() -> Vec<usize> {
    let nyq_freq = SR / 2;
    let freq_width = SR as f32 / FFT_SIZE as f32;
    let erb_low = freq2erb(0.);
    let erb_high = freq2erb(nyq_freq as f32);
    let mut erb = vec![0usize; NB_ERB];
    let step = (erb_high - erb_low) / NB_ERB as f32;
    let min_nb_freqs = MIN_NB_ERB_FREQS as i32;
    let mut prev_freq = 0i32;
    let mut freq_over = 0i32;
    for i in 1..=NB_ERB {
        let f = erb2freq(erb_low + i as f32 * step);
        let fb = (f / freq_width).round() as i32;
        let mut nb_freqs = fb - prev_freq - freq_over;
        if nb_freqs < min_nb_freqs {
            freq_over = min_nb_freqs - nb_freqs;
            nb_freqs = min_nb_freqs;
        } else {
            freq_over = 0;
        }
        erb[i - 1] = nb_freqs as usize;
        prev_freq = fb;
    }
    erb[NB_ERB - 1] += 1; // FREQ_SIZE is fft/2 + 1
    let sum: usize = erb.iter().sum();
    if sum > FREQ_SIZE {
        erb[NB_ERB - 1] -= sum - FREQ_SIZE;
    }
    debug_assert_eq!(erb.iter().sum::<usize>(), FREQ_SIZE);
    erb
}

/// The exponential smoothing factor for the running-mean feature normalisation.
///
/// Upstream `calc_norm_alpha` rounds to the fewest decimals that keep `alpha < 1`, which at
/// 48 kHz / 480 hop / tau 1 s lands on exactly 0.99. Reproduced (not hardcoded) so a future
/// hop-size change stays correct.
pub fn norm_alpha() -> f32 {
    let dt = HOP_SIZE as f32 / SR as f32;
    let alpha = (-dt / NORM_TAU).exp();
    let mut a = 1.0f32;
    let mut precision = 3i32;
    while a >= 1.0 {
        let p = 10f32.powi(precision);
        a = (alpha * p).round() / p;
        precision += 1;
    }
    a
}

/// STFT/ISTFT state plus the two running-mean normalisation states the features need.
///
/// One per channel: left and right stay independent, and the whole thing is a pure function
/// of the input (no entropy source), so a re-render is bit-identical (ADR-003).
pub struct Stft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    wnorm: f32,
    erb: Vec<usize>,
    alpha: f32,
    analysis_mem: Vec<f32>,
    synthesis_mem: Vec<f32>,
    fwd_scratch: Vec<Complex32>,
    inv_scratch: Vec<Complex32>,
    mean_norm_state: Vec<f32>,
    unit_norm_state: Vec<f32>,
}

impl Stft {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FFT_SIZE);
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let fwd_scratch = forward.make_scratch_vec();
        let inv_scratch = inverse.make_scratch_vec();

        // Vorbis window: sin(pi/2 * sin^2(pi*(n+0.5)/N)). Power-complementary at 50% overlap,
        // so applying it on both analysis and synthesis sums back to unity.
        let pi = std::f64::consts::PI;
        let half = (FFT_SIZE / 2) as f64;
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                let s = (0.5 * pi * (i as f64 + 0.5) / half).sin();
                (0.5 * pi * s * s).sin() as f32
            })
            .collect();

        // Applied on analysis only; makes irfft(rfft(x)) an identity (realfft is unnormalised).
        let wnorm = 1. / (FFT_SIZE.pow(2) as f32 / (2 * HOP_SIZE) as f32);

        let mut this = Self {
            forward,
            inverse,
            window,
            wnorm,
            erb: erb_widths(),
            alpha: norm_alpha(),
            analysis_mem: vec![0.; FFT_SIZE - HOP_SIZE],
            synthesis_mem: vec![0.; FFT_SIZE - HOP_SIZE],
            fwd_scratch,
            inv_scratch,
            mean_norm_state: Vec::new(),
            unit_norm_state: Vec::new(),
        };
        this.reset();
        this
    }

    /// Back to a clean start: buffers zeroed, normalisation states re-seeded. Two runs from
    /// a freshly reset state produce identical output.
    pub fn reset(&mut self) {
        self.analysis_mem.fill(0.);
        self.synthesis_mem.fill(0.);
        self.mean_norm_state = linspace(MEAN_NORM_INIT[0], MEAN_NORM_INIT[1], NB_ERB);
        self.unit_norm_state = linspace(UNIT_NORM_INIT[0], UNIT_NORM_INIT[1], NB_DF);
    }

    /// ERB band widths (bins per band).
    pub fn erb(&self) -> &[usize] {
        &self.erb
    }

    /// One analysis frame: `input` (HOP_SIZE samples) -> `output` (FREQ_SIZE complex bins).
    pub fn analysis(&mut self, input: &[f32], output: &mut [Complex32]) {
        debug_assert_eq!(input.len(), HOP_SIZE);
        debug_assert_eq!(output.len(), FREQ_SIZE);
        let mut buf = self.forward.make_input_vec();
        let (buf_first, buf_second) = buf.split_at_mut(FFT_SIZE - HOP_SIZE);
        let (win_first, win_second) = self.window.split_at(FFT_SIZE - HOP_SIZE);
        for ((x, &y), &w) in buf_first
            .iter_mut()
            .zip(self.analysis_mem.iter())
            .zip(win_first.iter())
        {
            *x = y * w;
        }
        for ((x, &y), &w) in buf_second
            .iter_mut()
            .zip(input.iter())
            .zip(win_second.iter())
        {
            *x = y * w;
        }
        // analysis_mem is exactly one hop (50% overlap), so it is a straight copy.
        self.analysis_mem.copy_from_slice(input);
        self.forward
            .process_with_scratch(&mut buf, output, &mut self.fwd_scratch)
            .expect("rfft forward");
        for x in output.iter_mut() {
            *x *= self.wnorm;
        }
    }

    /// One synthesis frame: `input` (FREQ_SIZE bins, consumed) -> `output` (HOP_SIZE samples).
    pub fn synthesis(&mut self, input: &mut [Complex32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), FREQ_SIZE);
        debug_assert_eq!(output.len(), HOP_SIZE);
        // realfft rejects a non-real DC/Nyquist bin; the DF stage can leave a whisker of
        // imaginary part there, so clear it rather than error out (upstream ignores the error).
        input[0].im = 0.;
        input[FREQ_SIZE - 1].im = 0.;
        let mut x = self.inverse.make_output_vec();
        self.inverse
            .process_with_scratch(input, &mut x, &mut self.inv_scratch)
            .expect("rfft inverse");
        for (x, &w) in x.iter_mut().zip(self.window.iter()) {
            *x *= w;
        }
        let (x_first, x_second) = x.split_at(HOP_SIZE);
        for ((out, &xi), &mem) in output
            .iter_mut()
            .zip(x_first.iter())
            .zip(self.synthesis_mem.iter())
        {
            *out = xi + mem;
        }
        self.synthesis_mem.copy_from_slice(x_second);
    }

    /// ERB feature for one frame: band energy in dB, running-mean normalised, scaled by 1/40.
    pub fn feat_erb(&mut self, spec: &[Complex32], out: &mut [f32]) {
        debug_assert_eq!(out.len(), NB_ERB);
        let mut bcsum = 0usize;
        for (&width, o) in self.erb.iter().zip(out.iter_mut()) {
            let k = 1. / width as f32;
            let mut acc = 0f32;
            for j in 0..width {
                let x = spec[bcsum + j];
                acc += (x.re * x.re + x.im * x.im) * k;
            }
            *o = acc;
            bcsum += width;
        }
        for o in out.iter_mut() {
            *o = (*o + 1e-10).log10() * 10.;
        }
        let alpha = self.alpha;
        for (x, s) in out.iter_mut().zip(self.mean_norm_state.iter_mut()) {
            *s = *x * (1. - alpha) + *s * alpha;
            *x -= *s;
            *x /= 40.;
        }
    }

    /// Complex feature for one frame: the first [`NB_DF`] bins, unit-norm normalised.
    pub fn feat_spec(&mut self, spec: &[Complex32], out: &mut [Complex32]) {
        debug_assert_eq!(out.len(), NB_DF);
        let alpha = self.alpha;
        for ((x, &s_in), st) in out
            .iter_mut()
            .zip(spec[..NB_DF].iter())
            .zip(self.unit_norm_state.iter_mut())
        {
            *st = s_in.norm() * (1. - alpha) + *st * alpha;
            *x = s_in / st.sqrt();
        }
    }
}

impl Default for Stft {
    fn default() -> Self {
        Self::new()
    }
}

fn linspace(min: f32, max: f32, n: usize) -> Vec<f32> {
    let step = (max - min) / (n - 1) as f32;
    (0..n).map(|i| min + i as f32 * step).collect()
}

/// Number of STFT frames for `len` samples, including the one flush frame that pushes the
/// OLA tail out (see the module docs on the 480-sample analysis latency).
pub fn n_frames(len: usize) -> usize {
    len.div_ceil(HOP_SIZE) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erb_widths_sum_to_freq_size() {
        assert_eq!(erb_widths().iter().sum::<usize>(), FREQ_SIZE);
        assert_eq!(erb_widths().len(), NB_ERB);
        // The first bands are floored at the two-bin minimum.
        assert_eq!(erb_widths()[0], MIN_NB_ERB_FREQS);
    }

    #[test]
    fn norm_alpha_matches_upstream() {
        // 48 kHz / 480 hop / tau 1 s -> exp(-0.01) rounded to 3 decimals.
        assert_eq!(norm_alpha(), 0.99);
    }

    /// The whole point of the STFT port: analysis -> synthesis with an untouched spectrum has
    /// to give the input back, sample-aligned, or every downstream number is off.
    #[test]
    fn stft_istft_roundtrip_is_identity() {
        let len = 4801; // deliberately not a multiple of the hop
        let input: Vec<f32> = (0..len)
            .map(|i| (i as f32 * 0.017).sin() * 0.4 + (i as f32 * 0.31).cos() * 0.2)
            .collect();

        let mut stft = Stft::new();
        let frames = n_frames(len);
        let mut padded = input.clone();
        padded.resize(frames * HOP_SIZE, 0.);

        let mut out = vec![0f32; frames * HOP_SIZE];
        let mut spec = vec![Complex32::default(); FREQ_SIZE];
        for k in 0..frames {
            let lo = k * HOP_SIZE;
            stft.analysis(&padded[lo..lo + HOP_SIZE], &mut spec);
            // no processing at all
            let mut s = spec.clone();
            stft.synthesis(&mut s, &mut out[lo..lo + HOP_SIZE]);
        }
        // Output lags the input by exactly one hop (fft_size - hop_size = 480).
        let recon = &out[HOP_SIZE..HOP_SIZE + len];
        let err = recon
            .iter()
            .zip(input.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(err < 1e-4, "STFT round-trip error {err} is too large");
    }
}
