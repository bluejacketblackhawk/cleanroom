//! Late-reverberation suppression — the Studio tier's "strong dereverb" (03 §4.4, §2 "RT60 >
//! 0.5 s -> recommend Studio tier").
//!
//! DFN3 takes the edge off mild reverb but it is a denoiser, not a dereverberator. This stage
//! adds an explicit one: Lebart's statistical model of late reverberation. The tail of a room
//! response decays exponentially, so the late-reverb power arriving in frame `t` is well
//! approximated by an attenuated, delayed copy of the signal's own power spectrum:
//!
//! ```text
//! P_late(t, f) = decay^(D) · P(t − D, f),   decay = exp(−3·ln(10)·hop / (RT60·sr))
//! ```
//!
//! with `D` the pre-delay in frames (the direct sound plus early reflections, which we keep —
//! they are what makes a voice sound present rather than dead). A Wiener-style gain then pulls
//! the estimated late energy out, floored so we never gate a tail into a hole.
//!
//! `rt60` is estimated blind from the decay of the signal's own energy envelope, so the stage
//! is self-tuning and does nothing on already-dry material (which is the point: it is in the
//! Studio chain unconditionally and must not hurt a dry room).
//!
//! Deterministic: pure DSP over the STFT, no RNG, no adaptive state carried between renders.

use num_complex::Complex32;

use crate::stft::{FREQ_SIZE, HOP_SIZE, SR};

/// Frames of direct sound + early reflections to protect, at a 10 ms hop.
///
/// This has to be *long*. A sustained vowel holds its power for 150–250 ms, so a short
/// pre-delay makes `P(t − D)` a proxy for `P(t)` and the subtraction eats the vowel instead of
/// its tail — measured, a 50 ms pre-delay cost 0.35 DNSMOS SIG. At 200 ms we are safely past
/// the sustain and into genuinely late energy (which is also the conventional early/late
/// boundary).
const PRE_DELAY_FRAMES: usize = 20;
/// Gain floor: the most we ever pull down a bin (−9 dB). Deeper and tails turn into musical
/// noise, which measures well and sounds awful.
const GAIN_FLOOR: f32 = 0.35;
/// Below this RT60 the room is already dry and we do nothing at all.
const DRY_RT60_S: f32 = 0.25;
/// Sanity clamp on the blind RT60 estimate.
const MAX_RT60_S: f32 = 1.5;

/// Suppress late reverberation in a spectrogram, in place.
///
/// `spec` is the **enhanced** spectrogram (`frames × FREQ_SIZE`) and `amount` is 0..1 (0 =
/// bypass). `rt60` must be estimated from the **noisy** spectrogram — see [`estimate_rt60`] —
/// because by the time DFN3 has run, the tails it is supposed to measure have already been
/// gated away and every room reads as dry.
///
/// Returns the RT60 it was given, for the Health Card.
pub fn suppress(spec: &mut [Complex32], frames: usize, amount: f32, rt60: f32) -> f32 {
    let amount = amount.clamp(0.0, 1.0);
    if amount <= 0.0 || frames <= PRE_DELAY_FRAMES || rt60 < DRY_RT60_S {
        return rt60;
    }

    // Per-frame decay of the reverb tail, from the RT60 definition (−60 dB over RT60).
    let hop_s = HOP_SIZE as f32 / SR as f32;
    let decay = (-3.0 * std::f32::consts::LN_10 * hop_s / rt60).exp();
    let attenuation = decay.powi(PRE_DELAY_FRAMES as i32);

    // Snapshot the power spectrum first: the estimate must come from the *unprocessed* signal,
    // or the suppression eats its own tail.
    let power: Vec<f32> = spec.iter().map(|x| x.norm_sqr()).collect();

    for t in PRE_DELAY_FRAMES..frames {
        let src = (t - PRE_DELAY_FRAMES) * FREQ_SIZE;
        let dst = t * FREQ_SIZE;
        for f in 0..FREQ_SIZE {
            let p = power[dst + f];
            if p <= 1e-20 {
                continue;
            }
            let late = attenuation * power[src + f];
            // Wiener gain on the power ratio, then scaled by `amount` and floored.
            let gain = ((p - late) / p).max(0.0).sqrt();
            let gain = (1.0 - amount) + amount * gain;
            spec[dst + f] *= gain.max(GAIN_FLOOR);
        }
    }
    rt60
}

/// Blind RT60 estimate: fit an exponential decay to the decaying stretches of the broadband
/// energy envelope (a coarse Schroeder-style read on speech offsets, 03 §1).
///
/// **Feed this the noisy spectrogram, not the enhanced one.** A denoiser gates the gaps to
/// silence, which turns every decay into a cliff and makes even a cathedral read as dry.
///
/// Coarse on purpose — we only need the right decade to set the tail's decay constant, and a
/// wrong-but-small RT60 fails safe (less suppression, never more).
pub fn estimate_rt60(spec: &[Complex32], frames: usize) -> f32 {
    if frames < 20 {
        return 0.0;
    }
    // Broadband energy envelope, in dB.
    let env: Vec<f32> = (0..frames)
        .map(|t| {
            let e: f32 = spec[t * FREQ_SIZE..(t + 1) * FREQ_SIZE]
                .iter()
                .map(|x| x.norm_sqr())
                .sum();
            10.0 * (e + 1e-12).log10()
        })
        .collect();

    let peak = env.iter().cloned().fold(f32::MIN, f32::max);
    let floor = peak - 60.0;

    // Decay slopes measured over runs that fall monotonically from a local peak. We take the
    // median slope, which is robust to the odd cough or door slam.
    let hop_s = HOP_SIZE as f32 / SR as f32;
    let mut slopes: Vec<f32> = Vec::new();
    let mut t = 1usize;
    while t < frames {
        if env[t] < env[t - 1] && env[t - 1] > floor + 20.0 {
            let start = t - 1;
            let mut end = t;
            while end + 1 < frames && env[end + 1] < env[end] && env[end] > floor {
                end += 1;
            }
            let span = end - start;
            if span >= 5 {
                let drop = env[start] - env[end]; // positive dB
                let secs = span as f32 * hop_s;
                if drop > 3.0 && secs > 0.0 {
                    slopes.push(drop / secs); // dB per second
                }
            }
            t = end + 1;
        } else {
            t += 1;
        }
    }
    if slopes.is_empty() {
        return 0.0;
    }
    slopes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = slopes[slopes.len() / 2];
    if median <= 0.0 {
        return 0.0;
    }
    (60.0 / median).clamp(0.0, MAX_RT60_S)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dry signal (no tail) must come out untouched: Studio runs this stage unconditionally,
    /// so a false positive would smear every clean recording.
    #[test]
    fn dry_material_is_left_alone() {
        let frames = 200;
        let mut spec = vec![Complex32::default(); frames * FREQ_SIZE];
        // Abrupt on/off bursts: energy drops to nothing in one frame -> a huge dB/s slope ->
        // a tiny RT60 -> below DRY_RT60_S -> bypass.
        for t in 0..frames {
            let on = (t / 10) % 2 == 0;
            for f in 0..FREQ_SIZE {
                spec[t * FREQ_SIZE + f] = if on {
                    Complex32::new(0.1, 0.0)
                } else {
                    Complex32::new(1e-6, 0.0)
                };
            }
        }
        let before = spec.clone();
        let rt60 = estimate_rt60(&spec, frames);
        assert!(
            rt60 < DRY_RT60_S,
            "abrupt decays must read as dry, got {rt60}"
        );
        suppress(&mut spec, frames, 1.0, rt60);
        assert_eq!(before, spec, "a dry signal must pass through untouched");
    }

    /// A synthetic exponential tail should be both detected and pulled down.
    #[test]
    fn a_reverb_tail_is_detected_and_suppressed() {
        let frames = 400;
        let hop_s = HOP_SIZE as f32 / SR as f32;
        let rt60_true = 0.8f32;
        let decay = (-3.0 * std::f32::consts::LN_10 * hop_s / rt60_true).exp();

        // 10 bursts, each followed by an exponentially decaying tail.
        let mut spec = vec![Complex32::default(); frames * FREQ_SIZE];
        let mut level = 0f32;
        for t in 0..frames {
            if t % 40 == 0 {
                level = 1.0;
            } else {
                level *= decay;
            }
            for f in 0..FREQ_SIZE {
                spec[t * FREQ_SIZE + f] = Complex32::new(level, 0.0);
            }
        }
        let est = estimate_rt60(&spec, frames);
        assert!(
            (est - rt60_true).abs() < 0.25,
            "RT60 estimate {est} is too far from {rt60_true}"
        );

        let tail_before: f32 = (30..40).map(|t| spec[t * FREQ_SIZE].norm()).sum::<f32>();
        suppress(&mut spec, frames, 1.0, est);
        let tail_after: f32 = (30..40).map(|t| spec[t * FREQ_SIZE].norm()).sum::<f32>();
        assert!(
            tail_after < tail_before * 0.5,
            "the tail should be pulled down: {tail_before} -> {tail_after}"
        );
    }
}
