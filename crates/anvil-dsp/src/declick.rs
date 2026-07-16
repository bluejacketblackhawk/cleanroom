//! De-click / de-crackle (03 §4.3) — transient-outlier detection + short interpolation.
//!
//! Two processors share one detector core, because they are the same idea at two settings:
//!
//! - [`MouthDeClick`] (M3, "gentle mode"): saliva/lip smacks are extremely short,
//!   high-amplitude glitches that stand out from the surrounding waveform — spikes in the
//!   second difference of the signal (a sharp jump away from the local trend and back). We
//!   flag samples whose second difference is a strong outlier versus a **global** robust
//!   (median-absolute-deviation) estimate of scale, then bridge each ≤ 2 ms glitch with linear
//!   interpolation between the clean samples on either side.
//! - [`DeCrackle`] (M4): the broader crackle / impulse-noise case — vinyl-style ticks, USB
//!   dropouts, buffer glitches, bit errors. Same second-difference outlier test, but scored
//!   against **local statistics**: each sample's |d²| is judged against the mean |d²| of its
//!   own ±1.3 ms neighborhood (with a robust-sigma floor underneath), so a tick riding a quiet
//!   passage is as detectable as one in a shouted line — and, just as important, so that *loud*
//!   speech is never mistaken for damage. The repair is **AR-model interpolation**
//!   ([`crate::ar`]) rather than a straight line, so the bridge keeps the formant structure and
//!   pitch pulse instead of punching a spectral hole into the voice.
//!
//! Bounded gap length + a high outlier threshold keep both off musical transients (the
//! auto-decision also disables them in music mode, 03 §4.5).
//!
//! Detection runs on the mono downmix so every channel is repaired over the same index range,
//! preserving the stereo image; the repair itself is per-channel (each channel is bridged from
//! its own clean context). Deterministic (ADR-003): the scale is an order statistic, all math
//! is sequential float, no entropy.

use anvil_core::HOP_SAMPLES;
use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::ar;
use crate::Processor;

/// Longest glitch the gentle mouth de-click will bridge: 2 ms (03 §4.3, "≤ 2 ms gaps").
const MAX_GAP_MS: f32 = 2.0;
/// Longest glitch de-crackle will bridge: 1 ms. Crackle is impulsive — anything longer is
/// program material, and bridging it would smear speech (03 §4.3).
const MAX_CRACKLE_GAP_MS: f32 = 1.0;
/// MAD → σ scaling for a normal distribution (robust standard-deviation estimate).
const MAD_TO_SIGMA: f32 = 1.4826;
/// De-crackle local-statistics window: 2048 samples ≈ 43 ms @ 48 kHz — long enough for a
/// stable median, short enough to track a passage rather than the whole file.
const LOCAL_WINDOW: usize = 2_048;
/// Half-width of the neighborhood the de-crackle baseline is averaged over (≈ 1.3 ms each way).
const BASELINE_HALF_WIDTH: usize = 32;
/// Samples around the candidate excluded from its own baseline, so an impulse cannot inflate
/// the very statistic it is being judged against.
const BASELINE_EXCLUDE: usize = 4;
/// An impulse must clear this many robust sigmas of the *window* on top of the local-baseline
/// ratio test — the belt to the ratio test's braces, keeping us off noise-floor wiggle.
const SIGMA_FLOOR: f32 = 4.0;
/// Samples added to each side of a detected crackle run before repair. A rectangular tick only
/// trips the second-difference test on its *edges* (the interior of a flat 3-sample tick has
/// zero curvature), so the raw run under-covers the damage.
const CRACKLE_DILATE: usize = 2;
/// AR model order for the de-crackle repair (≈ 0.5 ms of prediction memory @ 48 kHz: enough
/// poles for the pitch pulse plus the first formants).
const AR_ORDER: usize = 24;
/// Clean context taken from each side of a gap for the AR fit.
const AR_CONTEXT: usize = 512;

/// Mouth de-click configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MouthDeClickConfig {
    /// Outlier threshold in robust sigmas. A sample is a click candidate when its second
    /// difference exceeds `sensitivity × σ̂`. Higher = gentler / fewer detections.
    pub sensitivity: f32,
    /// Longest glitch to bridge, in milliseconds (03: ≤ 2 ms).
    pub max_gap_ms: f32,
    /// Loudness gate: skip repair where the local RMS is below this dBFS (don't chase clicks
    /// in silence; speech-gated per 03 §4.3).
    pub gate_dbfs: f32,
}

impl Default for MouthDeClickConfig {
    fn default() -> Self {
        Self {
            sensitivity: 8.0,
            max_gap_ms: MAX_GAP_MS,
            gate_dbfs: -60.0,
        }
    }
}

/// De-crackle configuration (03 §4.3, the broad impulse-noise mode).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeCrackleConfig {
    /// Outlier threshold in robust sigmas, scored against the **local** window statistics.
    /// Lower than the mouth-click threshold because the local scale already adapts to the
    /// passage. Higher = gentler / fewer detections.
    pub sensitivity: f32,
    /// Longest impulse to bridge, in milliseconds (default 1 ms).
    pub max_gap_ms: f32,
    /// Loudness gate: skip repair where the local RMS is below this dBFS.
    pub gate_dbfs: f32,
}

impl Default for DeCrackleConfig {
    fn default() -> Self {
        Self {
            sensitivity: 6.0,
            max_gap_ms: MAX_CRACKLE_GAP_MS,
            gate_dbfs: -60.0,
        }
    }
}

/// Mouth de-click processor (03 §4.3 gentle mode; global scale + linear bridge).
#[derive(Debug, Clone)]
pub struct MouthDeClick {
    config: MouthDeClickConfig,
    sample_rate: f32,
}

impl MouthDeClick {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: MouthDeClickConfig) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
        }
    }

    /// The config.
    pub fn config(&self) -> MouthDeClickConfig {
        self.config
    }

    /// Detect click regions as `[start, end)` sample ranges on `mono`. A region begins at an
    /// outlier and extends while outliers continue, capped at `max_gap`. The threshold is a
    /// single global robust sigma (gentle mode: one high bar for the whole file).
    fn detect(&self, mono: &[f32]) -> Vec<(usize, usize)> {
        let n = mono.len();
        if n < 4 {
            return Vec::new();
        }
        let d2 = second_difference(mono);
        let sigma = robust_sigma(&d2[2..]);
        let threshold = self.config.sensitivity * sigma;
        let thresholds = vec![threshold; n];

        scan_outliers(
            mono,
            &d2,
            &thresholds,
            gap_samples(self.config.max_gap_ms, self.sample_rate),
            10f32.powf(self.config.gate_dbfs / 20.0),
        )
    }
}

impl Processor for MouthDeClick {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        if frames < 4 || buffer.channel_count() == 0 {
            return;
        }
        let mono = mono_downmix(buffer);
        let regions = self.detect(&mono);

        // Bridge each region with linear interpolation between the clean anchor samples just
        // outside it (index `start−1` and `end`). Interpolating rather than gating keeps the
        // repair inaudible — no hole, no zero-crossing click of its own.
        for (start, end) in regions {
            let left = start.saturating_sub(1);
            let right = end.min(frames - 1);
            for channel in buffer.planar_mut() {
                ar::linear_bridge(channel, left, right);
            }
        }
    }
}

/// De-crackle processor (03 §4.3, broad mode; local statistics + AR interpolation).
#[derive(Debug, Clone)]
pub struct DeCrackle {
    config: DeCrackleConfig,
    sample_rate: f32,
}

impl DeCrackle {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: DeCrackleConfig) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
        }
    }

    /// The config.
    pub fn config(&self) -> DeCrackleConfig {
        self.config
    }

    /// Detect impulse regions on `mono` (03 §4.3: "transient-outlier detection against local
    /// statistics"). Two tests, both of which must pass:
    ///
    /// 1. **Local-baseline ratio.** The sample's second difference must tower over the mean
    ///    |d²| of its own ± [`BASELINE_HALF_WIDTH`] neighborhood (excluding itself). This is
    ///    the test that separates an impulse from *program material*: voiced speech is smooth
    ///    at the sample scale, so its |d²| barely changes from one sample to the next and the
    ///    ratio stays near 1 no matter how loud it gets — whereas a tick is, by definition,
    ///    curvature that its neighbors know nothing about. A plain MAD threshold cannot make
    ///    that distinction: on a harmonic-rich voice the |d²| peaks of ordinary speech already
    ///    sit many MADs above the median, and the detector eats the speech.
    /// 2. **Sigma floor.** It must also clear [`SIGMA_FLOOR`] robust sigmas of its
    ///    [`LOCAL_WINDOW`], which keeps us off noise-floor wiggle where the baseline is
    ///    vanishingly small.
    fn detect(&self, mono: &[f32]) -> Vec<(usize, usize)> {
        let n = mono.len();
        if n < 4 {
            return Vec::new();
        }
        let d2 = second_difference(mono);
        let mags: Vec<f32> = d2.iter().map(|v| v.abs()).collect();

        // Prefix sums make the neighborhood mean O(1) per sample (an 8-hour file is 1.4 G
        // samples — a per-sample sort is not on the menu).
        let mut prefix = vec![0.0f64; n + 1];
        for i in 0..n {
            prefix[i + 1] = prefix[i] + mags[i] as f64;
        }
        let sum = |lo: usize, hi: usize| -> f64 { prefix[hi] - prefix[lo] };

        let mut thresholds = vec![f32::MAX; n];
        let mut w = 0;
        while w < n {
            let win_end = (w + LOCAL_WINDOW).min(n);
            let lo = w.max(2); // d2[0..2] is undefined
            let sigma_floor = if lo < win_end {
                SIGMA_FLOOR * robust_sigma(&d2[lo..win_end])
            } else {
                f32::MAX
            };
            for (i, slot) in thresholds
                .iter_mut()
                .enumerate()
                .take(win_end)
                .skip(w.max(2))
            {
                let blo = i.saturating_sub(BASELINE_HALF_WIDTH);
                let bhi = (i + BASELINE_HALF_WIDTH + 1).min(n);
                let elo = i.saturating_sub(BASELINE_EXCLUDE);
                let ehi = (i + BASELINE_EXCLUDE + 1).min(n);
                let count = (bhi - blo) - (ehi - elo);
                let baseline = if count > 0 {
                    ((sum(blo, bhi) - sum(elo, ehi)) / count as f64) as f32
                } else {
                    0.0
                };
                *slot = (self.config.sensitivity * baseline).max(sigma_floor);
            }
            w = win_end;
        }

        let regions = scan_outliers(
            mono,
            &d2,
            &thresholds,
            gap_samples(self.config.max_gap_ms, self.sample_rate),
            10f32.powf(self.config.gate_dbfs / 20.0),
        );
        let max_len = gap_samples(self.config.max_gap_ms, self.sample_rate) + 2 * CRACKLE_DILATE;
        dilate_and_merge(&regions, CRACKLE_DILATE, n, max_len)
    }
}

impl Processor for DeCrackle {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        if frames < 4 || buffer.channel_count() == 0 {
            return;
        }
        let mono = mono_downmix(buffer);
        let regions = self.detect(&mono);

        // Repair each channel from its own clean context: AR bridge first, straight line only
        // where the model has too little context or runs away.
        for (start, end) in regions {
            for channel in buffer.planar_mut() {
                match ar::interpolate_gap(channel, start, end, AR_ORDER, AR_CONTEXT) {
                    Some(patch) => channel[start..end].copy_from_slice(&patch),
                    None => {
                        ar::linear_bridge(channel, start.saturating_sub(1), end.min(frames - 1))
                    }
                }
            }
        }
    }
}

// ---- Shared detector core -----------------------------------------------------------------

/// Average all channels into a mono detection signal.
fn mono_downmix(buffer: &AudioBuffer) -> Vec<f32> {
    let frames = buffer.frames();
    let channels = buffer.channel_count().max(1);
    let inv_ch = 1.0 / channels as f32;
    let mut mono = vec![0.0f32; frames];
    for c in 0..buffer.channel_count() {
        for (i, &s) in buffer.channel(c).iter().enumerate() {
            mono[i] += s * inv_ch;
        }
    }
    mono
}

/// Second difference `d2[i] = x[i] − 2x[i−1] + x[i−2]` (0 for the first two samples). A click
/// is a jump away from the local trend and back, which this makes stand out from tonal
/// program material.
fn second_difference(x: &[f32]) -> Vec<f32> {
    let mut d2 = vec![0.0f32; x.len()];
    for i in 2..x.len() {
        d2[i] = x[i] - 2.0 * x[i - 1] + x[i - 2];
    }
    d2
}

/// Robust scale of a slice: MAD-about-zero × 1.4826 (σ̂ for a normal), floored so a silent or
/// perfectly periodic passage cannot produce a zero threshold.
fn robust_sigma(d2: &[f32]) -> f32 {
    let mut mags: Vec<f32> = d2.iter().map(|v| v.abs()).collect();
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if mags.is_empty() {
        0.0
    } else {
        mags[mags.len() / 2]
    };
    (MAD_TO_SIGMA * median).max(1e-6)
}

/// Convert a gap length in ms to samples (at least 1).
fn gap_samples(ms: f32, sample_rate: f32) -> usize {
    (((ms / 1000.0) * sample_rate).round() as usize).max(1)
}

/// Walk the second difference and collect `[start, end)` outlier runs, capped at `max_gap` and
/// gated on local RMS so we never chase "clicks" in silence.
fn scan_outliers(
    mono: &[f32],
    d2: &[f32],
    thresholds: &[f32],
    max_gap: usize,
    gate_lin: f32,
) -> Vec<(usize, usize)> {
    let n = mono.len();
    let mut regions = Vec::new();
    let mut i = 2;
    while i < n {
        if d2[i].abs() > thresholds[i] {
            let start = i;
            let mut end = i + 1;
            // Extend while outliers persist, but never beyond the max gap.
            while end < n && d2[end].abs() > thresholds[end] && (end - start) < max_gap {
                end += 1;
            }
            // Local RMS gate: only repair where there is signal (speech), not silence.
            let lo = start.saturating_sub(HOP_SAMPLES / 2);
            let hi = (end + HOP_SAMPLES / 2).min(n);
            if rms(&mono[lo..hi]) >= gate_lin {
                regions.push((start, end));
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    regions
}

/// Widen every region by `pad` samples on each side and merge the overlaps, dropping anything
/// that ends up longer than `max_len` (that is program material, not an impulse).
fn dilate_and_merge(
    regions: &[(usize, usize)],
    pad: usize,
    n: usize,
    max_len: usize,
) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(regions.len());
    for &(start, end) in regions {
        let start = start.saturating_sub(pad);
        let end = (end + pad).min(n);
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out.retain(|&(start, end)| end > start && end - start <= max_len);
    out
}

/// RMS of a slice (0 for empty).
fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.3 * (i as f32 * freq * TAU / 48_000.0).sin())
            .collect()
    }

    /// A crude but useful stand-in for speech: a pitched buzz (harmonic-rich) shaped into
    /// syllables, so a repair that punches a hole is measurable as excess error.
    fn speech_like(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let env = 0.5 + 0.5 * (t * 4.0 * TAU).sin(); // ~4 Hz syllabic envelope
                let voice = (t * 120.0 * TAU).sin()
                    + 0.5 * (t * 240.0 * TAU).sin()
                    + 0.25 * (t * 480.0 * TAU).sin()
                    + 0.12 * (t * 960.0 * TAU).sin();
                0.15 * env * voice
            })
            .collect()
    }

    // ---- Mouth de-click (M3 behavior — must not regress) ----------------------------------

    #[test]
    fn interpolates_out_an_injected_click() {
        let mut s = tone(200.0, 48_000);
        let clean = s.clone();
        // Inject a sharp click: a big spike over a couple of samples mid-file.
        let pos = 24_000;
        s[pos] += 0.9;
        s[pos + 1] -= 0.8;

        let mut buf = AudioBuffer::from_planar(vec![s], 48_000);
        MouthDeClick::new(48_000, MouthDeClickConfig::default()).process(&mut buf);

        // The click region should be pulled back toward the underlying tone.
        let repaired = buf.channel(0);
        let err_before = (0.9f32).max(0.8);
        let err_after = (repaired[pos] - clean[pos])
            .abs()
            .max((repaired[pos + 1] - clean[pos + 1]).abs());
        assert!(
            err_after < err_before * 0.5,
            "click should be attenuated: before≈{err_before}, after={err_after}"
        );
    }

    #[test]
    fn clean_tone_is_essentially_untouched() {
        let s = tone(200.0, 48_000);
        let mut buf = AudioBuffer::from_planar(vec![s.clone()], 48_000);
        MouthDeClick::new(48_000, MouthDeClickConfig::default()).process(&mut buf);
        let diff = buf
            .channel(0)
            .iter()
            .zip(&s)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(diff < 1e-3, "clean tone altered by {diff}");
    }

    #[test]
    fn deterministic() {
        let mut s = tone(200.0, 48_000);
        s[10_000] += 0.9;
        let make = || AudioBuffer::from_planar(vec![s.clone(), s.clone()], 48_000);
        let (mut a, mut b) = (make(), make());
        MouthDeClick::new(48_000, MouthDeClickConfig::default()).process(&mut a);
        MouthDeClick::new(48_000, MouthDeClickConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }

    // ---- De-crackle (M4) ------------------------------------------------------------------

    #[test]
    fn crackle_is_removed_without_smearing_speech() {
        let clean = speech_like(48_000 * 2);

        // Inject 40 crackle impulses (1–3 samples each) at deterministic positions.
        let mut dirty = clean.clone();
        let mut seed = 0x5EED_1234u32;
        let mut positions = Vec::new();
        for k in 0..40 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let pos = 2_000 + k * 1_100 + (seed >> 24) as usize % 200;
            let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
            for j in 0..1 + (k % 3) {
                dirty[pos + j] += sign * 0.6;
            }
            positions.push(pos);
        }

        let err_before = rms_err(&dirty, &clean);
        let mut buf = AudioBuffer::from_planar(vec![dirty], 48_000);
        DeCrackle::new(48_000, DeCrackleConfig::default()).process(&mut buf);
        let err_after = rms_err(buf.channel(0), &clean);

        assert!(
            err_after < err_before * 0.25,
            "crackle should be largely removed: before={err_before:.5}, after={err_after:.5}"
        );

        // And the speech between the impulses must survive: away from every repaired impulse
        // the signal is essentially bit-clean (no smearing).
        let mut damaged = vec![false; clean.len()];
        for &p in &positions {
            for d in damaged
                .iter_mut()
                .take((p + 64).min(clean.len()))
                .skip(p - 64)
            {
                *d = true;
            }
        }
        let untouched_err = buf
            .channel(0)
            .iter()
            .zip(&clean)
            .zip(&damaged)
            .filter(|(_, &d)| !d)
            .map(|((a, b), _)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            untouched_err < 1e-6,
            "de-crackle must not touch clean speech: max diff {untouched_err}"
        );
    }

    #[test]
    fn clean_speech_is_left_alone() {
        let s = speech_like(48_000);
        let mut buf = AudioBuffer::from_planar(vec![s.clone()], 48_000);
        DeCrackle::new(48_000, DeCrackleConfig::default()).process(&mut buf);
        let diff = buf
            .channel(0)
            .iter()
            .zip(&s)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(diff < 1e-3, "clean speech altered by {diff}");
    }

    #[test]
    fn decrackle_is_deterministic() {
        let mut s = speech_like(48_000);
        s[20_000] += 0.7;
        s[33_333] -= 0.6;
        let make = || AudioBuffer::from_planar(vec![s.clone(), s.clone()], 48_000);
        let (mut a, mut b) = (make(), make());
        DeCrackle::new(48_000, DeCrackleConfig::default()).process(&mut a);
        DeCrackle::new(48_000, DeCrackleConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }

    fn rms_err(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        if n == 0 {
            return 0.0;
        }
        (a[..n]
            .iter()
            .zip(&b[..n])
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            / n as f32)
            .sqrt()
    }
}
