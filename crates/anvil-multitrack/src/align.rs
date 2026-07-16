//! Alignment (03 §6): **GCC-PHAT** cross-correlation on 60 s windows → a constant offset
//! (double-enders), plus a **drift line** for cheap recorders whose clocks disagree
//! (≤ 50 ppm repaired by resampling). Confidence is surfaced per track.
//!
//! # The model
//!
//! Track *B* was recorded on a different machine than the reference *A*. Its samples are
//!
//! ```text
//!     B[n] = A[(1 − e)·n − D]
//! ```
//!
//! where `D` is the constant offset in samples (the recorders were started at different
//! moments) and `e` is the clock error (B's sample clock runs slow/fast relative to A's).
//! B therefore lags A by `D + e·n` — a **line**, not a constant: the whole point of the
//! drift term. Measure that line and you have both numbers:
//!
//! - measure the lag in several windows (GCC-PHAT), each at its own position `n`;
//! - least-squares fit `lag(n) = D + e·n` → intercept = offset, slope × 1e6 = **ppm**;
//! - repair: resample B by `1/(1 − e)` (which makes the lag constant at `D`), then shift by `D`.
//!
//! # Why GCC-PHAT and not plain cross-correlation
//!
//! The two mics see the same room through different transfer functions, and one signal is a
//! reverberant, band-limited copy of the other. Plain correlation peaks are smeared by that
//! spectral colouring; PHAT (phase transform) divides out the magnitude spectrum and keeps
//! only the phase, so the correlation collapses to a near-delta at the true delay no matter
//! how the two paths are EQ'd. That is exactly the invariance we want here — and it also
//! gives us a free confidence number, because an uncorrelated pair produces no peak at all.

use anvil_media::AudioBuffer;
use realfft::num_complex::Complex32;
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};

use crate::vad::mono;

/// Alignment tuning (03 §6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlignConfig {
    /// Correlation window in seconds (03 §6: "60 s windows").
    pub window_secs: f64,
    /// How far apart two recorders may have been started, seconds (the coarse search range).
    pub max_offset_secs: f64,
    /// Search radius around the coarse offset when measuring the per-window lag, seconds.
    /// Must cover the accumulated drift over the file (50 ppm × 2 h ≈ 0.36 s).
    pub residual_search_secs: f64,
    /// Drift beyond this is *reported* but not repaired (03 §6: "≤ 50 ppm resample repair").
    pub max_drift_ppm: f64,
    /// Below this, drift is not worth a resample (and would be within the estimator's noise).
    pub min_repair_ppm: f64,
    /// Below this confidence the offset is not trusted: no shift, no resample, and a warning
    /// in the mix report.
    pub min_confidence: f32,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            window_secs: 60.0,
            max_offset_secs: 30.0,
            residual_search_secs: 0.5,
            max_drift_ppm: 50.0,
            min_repair_ppm: 2.0,
            min_confidence: 0.15,
        }
    }
}

/// The alignment of a track set against its reference (track 0). One entry per input track;
/// the reference's own entry is `(0.0, 0.0, 1.0)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alignment {
    /// How far each track *lags* the reference, in seconds. Positive = the track is late and
    /// must be advanced by this much to line up. This is the intercept of the drift line
    /// (the lag at t = 0), not a mid-file average.
    pub offsets_secs: Vec<f64>,
    /// Clock drift of each track relative to the reference, in parts per million. Positive =
    /// the track's clock runs slow, so it falls further behind as the file plays.
    pub drift_ppm: Vec<f64>,
    /// Per-track confidence in 0..1 (1 = a clean, unambiguous correlation peak).
    pub confidence: Vec<f32>,
    /// Index of the reference track (always 0 today).
    pub reference: usize,
}

impl Alignment {
    /// The identity alignment for `n` tracks (used when alignment is disabled).
    pub fn identity(n: usize) -> Self {
        Self {
            offsets_secs: vec![0.0; n],
            drift_ppm: vec![0.0; n],
            confidence: vec![1.0; n],
            reference: 0,
        }
    }
}

/// The result of one GCC-PHAT correlation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lag {
    /// Lag of `b` relative to `a` in samples (fractional, parabolically interpolated).
    /// Positive = `b` is delayed relative to `a`.
    pub samples: f64,
    /// 0..1. `1 − (second peak / peak)`: 1 = a single unambiguous spike, 0 = noise.
    pub confidence: f32,
}

/// Guard radius around the main peak when looking for the runner-up, in samples (≈ 1 ms).
/// A PHAT peak is a near-delta but not a single bin, so the runner-up must be sought outside
/// the peak's own skirt or the confidence would always read 0.
const PEAK_GUARD: usize = 48;

/// GCC-PHAT: the lag of `b` relative to `a`, searched over ±`max_shift` samples.
///
/// `r[k] = IFFT( A(f)·conj(B(f)) / |A(f)·conj(B(f))| )[k] = Σₙ a[n+k]·b[n]` (phase-only).
/// If `b[n] = a[n − D]` then `r` peaks at `k = −D`, so the reported lag is `−k*`.
pub(crate) fn gcc_phat(a: &[f32], b: &[f32], max_shift: usize) -> Lag {
    let n = a.len().max(b.len());
    if n == 0 || a.is_empty() || b.is_empty() {
        return Lag {
            samples: 0.0,
            confidence: 0.0,
        };
    }
    let max_shift = max_shift.clamp(1, n.saturating_sub(1).max(1));
    // Circular correlation aliases lag k with lag k − fft_len; keeping fft_len ≥ n + max_shift
    // pushes every alias outside the searched band, so the peak we find is the linear one.
    let fft_len = (n + max_shift + 1).next_power_of_two();

    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(fft_len);
    let c2r = planner.plan_fft_inverse(fft_len);

    let mut ina = r2c.make_input_vec();
    let mut inb = r2c.make_input_vec();
    ina[..a.len()].copy_from_slice(a);
    inb[..b.len()].copy_from_slice(b);

    let mut sa = r2c.make_output_vec();
    let mut sb = r2c.make_output_vec();
    if r2c.process(&mut ina, &mut sa).is_err() || r2c.process(&mut inb, &mut sb).is_err() {
        return Lag {
            samples: 0.0,
            confidence: 0.0,
        };
    }

    // PHAT weighting: keep the phase of the cross spectrum, throw the magnitude away.
    let mut cross: Vec<Complex32> = sa
        .iter()
        .zip(sb.iter())
        .map(|(x, y)| {
            let c = x * y.conj();
            let m = c.norm();
            if m > 1e-9 {
                c / m
            } else {
                Complex32::new(0.0, 0.0)
            }
        })
        .collect();
    // A real inverse transform needs a Hermitian spectrum: DC and Nyquist must be real. They
    // already are (both inputs are real); this just kills any float dust so realfft accepts it.
    cross[0].im = 0.0;
    if let Some(last) = cross.last_mut() {
        last.im = 0.0;
    }

    let mut corr = c2r.make_output_vec();
    if c2r.process(&mut cross, &mut corr).is_err() {
        return Lag {
            samples: 0.0,
            confidence: 0.0,
        };
    }

    // Search k ∈ [−max_shift, max_shift]; negative lags live at the top of the buffer.
    let idx_of = |k: isize| -> usize {
        if k >= 0 {
            k as usize
        } else {
            (fft_len as isize + k) as usize
        }
    };
    let mut best_k: isize = 0;
    let mut best = f32::NEG_INFINITY;
    for k in -(max_shift as isize)..=(max_shift as isize) {
        let v = corr[idx_of(k)].abs();
        if v > best {
            best = v;
            best_k = k;
        }
    }
    // Runner-up outside the peak's skirt → confidence.
    let mut second = 0.0f32;
    for k in -(max_shift as isize)..=(max_shift as isize) {
        if (k - best_k).unsigned_abs() <= PEAK_GUARD {
            continue;
        }
        second = second.max(corr[idx_of(k)].abs());
    }
    let confidence = if best > 0.0 {
        (1.0 - second / best).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Parabolic interpolation over the peak's neighbours → sub-sample lag.
    let y0 = corr[idx_of(best_k - 1)].abs();
    let y1 = best;
    let y2 = corr[idx_of(best_k + 1)].abs();
    let denom = y0 - 2.0 * y1 + y2;
    let delta = if denom.abs() > 1e-12 {
        (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };

    Lag {
        // r peaks at k = −D, so D = −(k* + δ).
        samples: -(best_k as f64 + delta as f64),
        confidence,
    }
}

/// `x[start .. start+len]`, zero-padded where that runs off either end.
fn slice_padded(x: &[f32], start: i64, len: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; len];
    for (i, slot) in out.iter_mut().enumerate() {
        let idx = start + i as i64;
        if idx >= 0 && (idx as usize) < x.len() {
            *slot = x[idx as usize];
        }
    }
    out
}

/// One track's alignment against the reference: the drift line `lag(n) = D + e·n`.
fn align_pair(
    reference: &[f32],
    other: &[f32],
    sample_rate: u32,
    cfg: &AlignConfig,
) -> (f64, f64, f32) {
    let sr = sample_rate as f64;
    let n = reference.len().min(other.len());
    if n == 0 {
        return (0.0, 0.0, 0.0);
    }

    // --- Coarse pass: where is the other recorder, roughly? -------------------------------
    let win = ((cfg.window_secs * sr) as usize).clamp(1, n);
    let max_shift = ((cfg.max_offset_secs * sr) as usize).max(1);
    let coarse = gcc_phat(&reference[..win], &other[..win], max_shift);
    let d0 = coarse.samples.round() as i64;

    // --- Drift line: one lag measurement per window, then a weighted least-squares fit ----
    // Each window of `other` is pre-shifted by the coarse offset, so the residual search only
    // has to cover the drift that has accumulated — a cheap FFT instead of a huge one.
    let n_win = (n / win).max(1);
    let res_shift = ((cfg.residual_search_secs * sr) as usize).max(1);
    let mut pts: Vec<(f64, f64, f32)> = Vec::with_capacity(n_win);
    for w in 0..n_win {
        let t = (w * win) as i64;
        let a = slice_padded(reference, t, win);
        let b = slice_padded(other, t + d0, win);
        let lag = gcc_phat(&a, &b, res_shift);
        // x is the position of the window centre **in the other track's timeline** — the `n`
        // of `lag(n) = D + e·n`. That is where the drift term is accumulating.
        let x = t as f64 + d0 as f64 + win as f64 / 2.0;
        pts.push((x, d0 as f64 + lag.samples, lag.confidence));
    }

    let conf_sum: f32 = pts.iter().map(|p| p.2).sum();
    let confidence = if pts.is_empty() {
        0.0
    } else {
        (conf_sum / pts.len() as f32).clamp(0.0, 1.0)
    };

    // Fewer than two usable windows ⇒ no line to fit; take the coarse offset and no drift.
    let usable: Vec<&(f64, f64, f32)> = pts.iter().filter(|p| p.2 > 0.0).collect();
    if usable.len() < 2 {
        let offset = pts.first().map(|p| p.1).unwrap_or(coarse.samples);
        return (offset / sr, 0.0, confidence.max(coarse.confidence * 0.5));
    }

    // Weighted LS fit of lag = D + e·x (weights = per-window confidence).
    let sw: f64 = usable.iter().map(|p| p.2 as f64).sum();
    let sx: f64 = usable.iter().map(|p| p.2 as f64 * p.0).sum();
    let sy: f64 = usable.iter().map(|p| p.2 as f64 * p.1).sum();
    let sxx: f64 = usable.iter().map(|p| p.2 as f64 * p.0 * p.0).sum();
    let sxy: f64 = usable.iter().map(|p| p.2 as f64 * p.0 * p.1).sum();
    let denom = sw * sxx - sx * sx;
    if denom.abs() < 1e-6 {
        let offset = usable[0].1;
        return (offset / sr, 0.0, confidence);
    }
    let slope = (sw * sxy - sx * sy) / denom;
    let intercept = (sy - slope * sx) / sw;
    let drift_ppm = slope * 1e6;

    // A wild slope means the windows disagreed (bad correlation, or a real edit, not a clock).
    // Report it, refuse to act on it, and knock the confidence down so the report says why.
    let (drift_ppm, confidence) = if drift_ppm.abs() > cfg.max_drift_ppm * 4.0 {
        (drift_ppm, confidence * 0.25)
    } else {
        (drift_ppm, confidence)
    };

    (intercept / sr, drift_ppm, confidence)
}

/// Align `buffers` against `buffers[0]` (03 §6). Constant offset + drift line + confidence.
pub fn align_buffers(buffers: &[AudioBuffer], cfg: &AlignConfig) -> Alignment {
    if buffers.is_empty() {
        return Alignment::identity(0);
    }
    let sample_rate = buffers[0].sample_rate();
    let monos: Vec<Vec<f32>> = buffers.iter().map(mono).collect();

    let mut offsets_secs = vec![0.0f64; buffers.len()];
    let mut drift_ppm = vec![0.0f64; buffers.len()];
    let mut confidence = vec![1.0f32; buffers.len()];
    for i in 1..buffers.len() {
        let (off, ppm, conf) = align_pair(&monos[0], &monos[i], sample_rate, cfg);
        offsets_secs[i] = off;
        drift_ppm[i] = ppm;
        confidence[i] = conf;
    }
    Alignment {
        offsets_secs,
        drift_ppm,
        confidence,
        reference: 0,
    }
}

/// Lanczos kernel half-width (taps = 2·A). 8 is transparent for the ≤ 50 ppm ratios we use.
const LANCZOS_A: i64 = 8;

/// Lanczos kernel.
fn lanczos(x: f64) -> f64 {
    let a = LANCZOS_A as f64;
    if x.abs() < 1e-12 {
        1.0
    } else if x.abs() >= a {
        0.0
    } else {
        let px = std::f64::consts::PI * x;
        (a * px.sin() * (px / a).sin()) / (px * px)
    }
}

/// Band-limited interpolation of `x` at the fractional position `pos`.
pub(crate) fn interpolate(x: &[f32], pos: f64) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let base = pos.floor() as i64;
    let frac = pos - base as f64;
    let mut acc = 0.0f64;
    for k in (1 - LANCZOS_A)..=LANCZOS_A {
        let idx = base + k;
        if idx < 0 || idx as usize >= x.len() {
            continue;
        }
        acc += x[idx as usize] as f64 * lanczos(frac - k as f64);
    }
    acc as f32
}

/// Resample a channel by reading it at `step` samples per output sample.
fn resample_channel(x: &[f32], step: f64) -> Vec<f32> {
    if x.is_empty() || !(step.is_finite() && step > 0.0) {
        return x.to_vec();
    }
    let out_len = (((x.len() - 1) as f64) / step).floor() as usize + 1;
    (0..out_len)
        .map(|m| interpolate(x, m as f64 * step))
        .collect()
}

/// Repair clock drift by resampling (03 §6: "≤ 50 ppm resample repair for cheap recorders").
///
/// With `B[n] = A[(1 − e)·n − D]`, reading B at `m/(1 − e)` gives `Brep[m] = A[m − D]`: the
/// drift is gone and what is left is the constant offset. `ppm` is `e × 1e6`.
pub fn repair_drift(buf: &AudioBuffer, ppm: f64) -> AudioBuffer {
    let e = ppm * 1e-6;
    if !e.is_finite() || (1.0 - e).abs() < 1e-6 || e == 0.0 {
        return buf.clone();
    }
    let step = 1.0 / (1.0 - e);
    let channels: Vec<Vec<f32>> = buf
        .planar()
        .iter()
        .map(|c| resample_channel(c, step))
        .collect();
    AudioBuffer::from_planar(channels, buf.sample_rate())
}

/// Apply a constant offset: `out[m] = buf[m + offset]`. A positive offset *advances* the
/// track (it was late); a negative one delays it by prepending silence.
pub fn apply_offset(buf: &AudioBuffer, offset_samples: i64) -> AudioBuffer {
    if offset_samples == 0 {
        return buf.clone();
    }
    let frames = buf.frames();
    let out_len = if offset_samples > 0 {
        frames.saturating_sub(offset_samples as usize)
    } else {
        frames + (-offset_samples) as usize
    };
    let channels: Vec<Vec<f32>> = buf
        .planar()
        .iter()
        .map(|c| {
            (0..out_len)
                .map(|m| {
                    let idx = m as i64 + offset_samples;
                    if idx >= 0 && (idx as usize) < c.len() {
                        c[idx as usize]
                    } else {
                        0.0
                    }
                })
                .collect()
        })
        .collect();
    AudioBuffer::from_planar(channels, buf.sample_rate())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::{add, at, buffer, delayed, drifted, noise, speechy, SR};

    #[test]
    fn gcc_phat_recovers_a_known_delay_to_the_sample() {
        let a = speechy(3.0, 7);
        let delay = 1234usize;
        let mut b = vec![0.0f32; a.len()];
        b[delay..].copy_from_slice(&a[..a.len() - delay]);
        // Attenuate it — bleed is a *quiet* copy — and add a little independent noise.
        for (i, s) in b.iter_mut().enumerate() {
            *s = *s * 0.15 + 0.0005 * ((i * 7919 % 1000) as f32 / 500.0 - 1.0);
        }
        let lag = gcc_phat(&a, &b, SR as usize);
        assert!(
            (lag.samples - delay as f64).abs() < 1.0,
            "expected {delay}, got {}",
            lag.samples
        );
        assert!(lag.confidence > 0.5, "confidence {}", lag.confidence);
    }

    #[test]
    fn gcc_phat_has_no_confidence_in_uncorrelated_tracks() {
        let a = speechy(2.0, 1);
        let b = speechy(2.0, 99);
        let lag = gcc_phat(&a, &b, SR as usize / 2);
        assert!(
            lag.confidence < 0.35,
            "uncorrelated tracks must not look aligned (got {})",
            lag.confidence
        );
    }

    /// Test (a), alignment half: a double-ender where B is a delayed, attenuated copy of A —
    /// the offset must come back to the sample.
    #[test]
    fn alignment_recovers_a_known_offset_to_the_sample() {
        let a = speechy(8.0, 3);
        let delay = 4321usize;
        let mut b = delayed(&a, delay, 0.15);
        add(&mut b, &noise(8.0, 0.0006, 42));

        let al = align_buffers(
            &[buffer(a), buffer(b)],
            &AlignConfig {
                max_offset_secs: 5.0,
                ..Default::default()
            },
        );
        // Measured: 4321.000 samples, confidence 0.998.
        let measured = al.offsets_secs[1] * SR as f64;
        assert!(
            (measured - delay as f64).abs() <= 1.0,
            "offset {measured:.2} samples, expected {delay}"
        );
        assert_eq!(al.offsets_secs[0], 0.0, "the reference does not move");
        assert!(al.confidence[1] > 0.5, "confidence {}", al.confidence[1]);
    }

    /// Test (c): a recorder whose clock is 40 ppm off. The drift line must be measured, and
    /// the resample repair must leave a *constant* lag behind — which is what "repaired" means.
    #[test]
    fn drift_is_measured_and_repaired() {
        let secs = 30.0;
        let ppm = 40.0;
        let delay = 2000.0;
        let a = speechy(secs, 5);
        let b = drifted(&a, ppm, delay, 0.5);

        let cfg = AlignConfig {
            window_secs: 3.0,
            max_offset_secs: 1.0,
            residual_search_secs: 0.1,
            ..Default::default()
        };
        // Measured: 39.33 ppm (truth 40), offset 2000.2 samples (truth 2000), confidence 0.91.
        let al = align_buffers(&[buffer(a.clone()), buffer(b.clone())], &cfg);
        assert!(
            (al.drift_ppm[1] - ppm).abs() < 5.0,
            "drift {:.1} ppm, expected {ppm}",
            al.drift_ppm[1]
        );
        assert!(
            (al.offsets_secs[1] * SR as f64 - delay).abs() < 20.0,
            "offset {:.0} samples, expected {delay}",
            al.offsets_secs[1] * SR as f64
        );

        // Repair, then re-measure: the residual drift must be gone and the track must line up.
        let repaired = repair_drift(&buffer(b), al.drift_ppm[1]);
        let shifted = apply_offset(&repaired, (al.offsets_secs[1] * SR as f64).round() as i64);
        // Measured after the repair: 1.01 ppm residual drift, −0.2 samples residual offset.
        let after = align_buffers(&[buffer(a), shifted], &cfg);
        assert!(
            after.drift_ppm[1].abs() < 4.0,
            "residual drift {:.1} ppm should be ~0 after the repair",
            after.drift_ppm[1]
        );
        assert!(
            (after.offsets_secs[1] * SR as f64).abs() < 20.0,
            "residual offset {:.0} samples should be ~0 after the shift",
            after.offsets_secs[1] * SR as f64
        );

        // And the *proof* the drift is really gone: without the resample the lag at the end of
        // the file differs from the lag at the start by ppm × duration ≈ 57 samples; with it,
        // start and end agree.
        let a2 = speechy(secs, 5);
        let rep = repair_drift(&buffer(drifted(&a2, ppm, delay, 0.5)), al.drift_ppm[1]);
        let rep = rep.channel(0);
        let head = gcc_phat(&a2[..at(3.0)], &rep[..at(3.0)], 4800);
        let tail_a = &a2[at(25.0)..at(28.0)];
        let tail_b = &rep[at(25.0)..at(28.0)];
        let tail = gcc_phat(tail_a, tail_b, 4800);
        assert!(
            (head.samples - tail.samples).abs() < 6.0,
            "lag drifted from {:.1} to {:.1} samples across the file — not repaired",
            head.samples,
            tail.samples
        );
    }

    #[test]
    fn interpolation_is_transparent_on_a_sine() {
        let f = 440.0f64;
        let x: Vec<f32> = (0..4800)
            .map(|i| ((i as f64) * f * std::f64::consts::TAU / SR as f64).sin() as f32)
            .collect();
        for &pos in &[1000.25f64, 2000.5, 3000.75] {
            let want = (pos * f * std::f64::consts::TAU / SR as f64).sin() as f32;
            assert!(
                (interpolate(&x, pos) - want).abs() < 1e-3,
                "interp at {pos}: {} vs {want}",
                interpolate(&x, pos)
            );
        }
    }
}
