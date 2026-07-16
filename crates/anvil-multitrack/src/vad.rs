//! Frame features, noise floors, and the energy VAD the multitrack stage runs on.
//!
//! The §1 analysis pass owns the "real" VAD, but its per-hop speech mask is internal to
//! `anvil-dsp` (only the *ratios* survive into [`anvil_dsp::AnalysisReport`]). The multitrack
//! stage needs a per-frame mask **per track**, so it computes its own: a percentile noise
//! floor plus a fixed margin, with hangover. Same family as §1's gate (energy + floor), and
//! it is deterministic — no time, no threads, no entropy (ADR-003).

use anvil_media::AudioBuffer;

/// Floor for log energy so a digital-silence frame is a number, not −inf.
const ENERGY_EPS: f32 = 1e-12;

/// Average all channels of `buf` into one mono signal (the analysis view of a track).
pub(crate) fn mono(buf: &AudioBuffer) -> Vec<f32> {
    let frames = buf.frames();
    let ch = buf.channel_count();
    if ch == 0 || frames == 0 {
        return Vec::new();
    }
    let mut out = vec![0.0f32; frames];
    for c in 0..ch {
        for (i, &s) in buf.channel(c).iter().enumerate() {
            out[i] += s;
        }
    }
    let inv = 1.0 / ch as f32;
    for s in &mut out {
        *s *= inv;
    }
    out
}

/// Mean-square level in dB of `x[start .. start+len]`, zero-padding out-of-range.
pub(crate) fn window_db(x: &[f32], start: isize, len: usize) -> f32 {
    if len == 0 {
        return 10.0 * ENERGY_EPS.log10();
    }
    let mut acc = 0.0f64;
    for i in 0..len {
        let idx = start + i as isize;
        if idx >= 0 && (idx as usize) < x.len() {
            let v = x[idx as usize] as f64;
            acc += v * v;
        }
    }
    let ms = (acc / len as f64) as f32;
    10.0 * (ms + ENERGY_EPS).log10()
}

/// Per-frame mean-square level in dB: frame `f` covers `x[f*hop .. f*hop+win]`.
pub(crate) fn frame_levels_db(x: &[f32], win: usize, hop: usize) -> Vec<f32> {
    if x.is_empty() || win == 0 || hop == 0 {
        return Vec::new();
    }
    let n = x.len().div_ceil(hop);
    (0..n)
        .map(|f| window_db(x, (f * hop) as isize, win))
        .collect()
}

/// `p`-quantile of `values` (p in 0..1), `default` when empty. Deterministic total order.
pub(crate) fn percentile(values: &[f32], p: f32, default: f32) -> f32 {
    if values.is_empty() {
        return default;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() - 1) as f32 * p.clamp(0.0, 1.0)).round() as usize;
    v[idx]
}

/// Noise floor of a track = 10th percentile of its frame levels (03 §1).
pub(crate) fn noise_floor_db(levels: &[f32]) -> f32 {
    percentile(levels, 0.10, -120.0)
}

/// Noise floor of `b` estimated **only on frames where no other speaker is active**.
///
/// This matters for the crossgate: on a bleed-contaminated track the global percentile can
/// land inside the spill and read the bleed as the floor, which then makes the bleed look
/// like B's own voice. Spill only exists while somebody else is talking, so the frames where
/// every other mic is quiet are the only honest sample of B's own floor. Falls back to the
/// global percentile when the other speakers never stop.
pub(crate) fn quiet_frame_floor_db(levels: &[f32], others_active: &[bool]) -> f32 {
    let quiet: Vec<f32> = levels
        .iter()
        .enumerate()
        .filter(|(f, _)| !others_active.get(*f).copied().unwrap_or(false))
        .map(|(_, &l)| l)
        .collect();
    if quiet.len() * 10 < levels.len() {
        // Fewer than 10% of frames are usable — not a reliable estimate; use the global one.
        noise_floor_db(levels)
    } else {
        noise_floor_db(&quiet)
    }
}

/// Speech gate: frame level above `floor + margin`.
pub(crate) fn speech_mask(levels: &[f32], floor_db: f32, margin_db: f32) -> Vec<bool> {
    levels.iter().map(|&l| l > floor_db + margin_db).collect()
}

/// Extend every `true` forward by `n` frames (hangover / hold).
pub(crate) fn dilate_forward(mask: &[bool], n: usize) -> Vec<bool> {
    let mut out = mask.to_vec();
    if n == 0 {
        return out;
    }
    let mut countdown = 0usize;
    for (i, slot) in out.iter_mut().enumerate() {
        if mask[i] {
            countdown = n;
        } else if countdown > 0 {
            *slot = true;
            countdown -= 1;
        }
    }
    out
}

/// Extend every `true` backward by `n` frames (lookahead).
pub(crate) fn dilate_backward(mask: &[bool], n: usize) -> Vec<bool> {
    let mut out = mask.to_vec();
    if n == 0 {
        return out;
    }
    let mut countdown = 0usize;
    for i in (0..out.len()).rev() {
        if mask[i] {
            countdown = n;
        } else if countdown > 0 {
            out[i] = true;
            countdown -= 1;
        }
    }
    out
}

/// Convert milliseconds to a whole number of frames at hop `hop` samples.
pub(crate) fn ms_to_frames(ms: f32, hop: usize, sample_rate: u32) -> usize {
    if hop == 0 {
        return 0;
    }
    let samples = ms as f64 * 1e-3 * sample_rate as f64;
    (samples / hop as f64).round().max(0.0) as usize
}

/// dB → linear.
#[inline]
pub(crate) fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_db_of_a_full_scale_square_is_zero() {
        let x = vec![1.0f32; 100];
        assert!((window_db(&x, 0, 100) - 0.0).abs() < 1e-3);
    }

    #[test]
    fn window_db_zero_pads_out_of_range() {
        let x = vec![1.0f32; 10];
        // Half in range, half padded → mean-square 0.5 → −3 dB.
        assert!((window_db(&x, 5, 10) + 3.0103).abs() < 1e-2);
    }

    #[test]
    fn dilation_grows_the_mask_in_the_right_direction() {
        let m = vec![false, false, true, false, false, false];
        assert_eq!(
            dilate_forward(&m, 2),
            vec![false, false, true, true, true, false]
        );
        assert_eq!(
            dilate_backward(&m, 2),
            vec![true, true, true, false, false, false]
        );
    }
}
