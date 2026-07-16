//! Linear sample-rate conversion for planar f32 material.
//!
//! Two call sites need it (both UI-independent):
//! - decode normalization: a decoded file at its native rate → the engine's internal
//!   [`anvil_core::INTERNAL_SAMPLE_RATE`] (the M0 wav seam; Lane B's decoder lands later).
//! - device reconciliation: the internal 48 kHz material → whatever rate the output
//!   device actually runs at (ADR-010 — the engine owns the rate mismatch, not the UI).
//!
//! Linear interpolation is deliberate for M0: cheap, allocation-light, and good enough
//! for monitoring playback. A polyphase resampler (rubato) is a later quality lever and
//! belongs behind the same signature so the swap is local.

/// Resample every channel of `planar` from `src_rate` to `dst_rate` with linear
/// interpolation. Channel count is preserved; each output channel has
/// `round(frames * dst_rate / src_rate)` samples.
///
/// Returns the input clone when the rates already match (the common device case) so no
/// arithmetic error creeps in. Empty or rate-zero input yields an empty-per-channel clone.
pub fn resample_planar(planar: &[Vec<f32>], src_rate: u32, dst_rate: u32) -> Vec<Vec<f32>> {
    if src_rate == dst_rate || src_rate == 0 || dst_rate == 0 {
        return planar.to_vec();
    }
    let src_frames = planar.first().map_or(0, Vec::len);
    if src_frames == 0 {
        return vec![Vec::new(); planar.len()];
    }

    // out_frames = src_frames * dst_rate / src_rate, computed in f64 to avoid overflow on
    // long files (a 3-hour 48 kHz file is > 500M frames).
    let ratio = dst_rate as f64 / src_rate as f64;
    let out_frames = ((src_frames as f64) * ratio).round() as usize;

    planar
        .iter()
        .map(|ch| {
            let mut out = Vec::with_capacity(out_frames);
            for i in 0..out_frames {
                // Position in source samples for output frame i.
                let src_pos = i as f64 / ratio;
                let i0 = src_pos.floor() as usize;
                let frac = (src_pos - i0 as f64) as f32;
                let s0 = ch.get(i0).copied().unwrap_or(0.0);
                // Clamp the upper tap to the last sample so the tail never reads past end.
                let s1 = ch.get(i0 + 1).copied().unwrap_or(s0);
                out.push(s0 + (s1 - s0) * frac);
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_rates_match() {
        let planar = vec![vec![0.1, -0.2, 0.3, -0.4]];
        let out = resample_planar(&planar, 48_000, 48_000);
        assert_eq!(out, planar);
    }

    #[test]
    fn doubling_rate_doubles_frame_count() {
        let planar = vec![vec![0.0, 1.0, 0.0, -1.0]];
        let out = resample_planar(&planar, 24_000, 48_000);
        // 4 frames at 2x → 8 frames.
        assert_eq!(out[0].len(), 8);
        // Linear midpoints land halfway between neighbours.
        assert!((out[0][0] - 0.0).abs() < 1e-6);
        assert!((out[0][1] - 0.5).abs() < 1e-6); // between 0.0 and 1.0
        assert!((out[0][2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn halving_rate_halves_frame_count() {
        let planar = vec![vec![0.0, 0.25, 0.5, 0.75, 1.0, 0.75, 0.5, 0.25]];
        let out = resample_planar(&planar, 48_000, 24_000);
        assert_eq!(out[0].len(), 4);
    }

    #[test]
    fn preserves_channel_count() {
        let planar = vec![vec![1.0, 2.0], vec![-1.0, -2.0]];
        let out = resample_planar(&planar, 44_100, 48_000);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), out[1].len());
    }

    #[test]
    fn empty_input_is_empty_per_channel() {
        let planar = vec![Vec::new(), Vec::new()];
        let out = resample_planar(&planar, 44_100, 48_000);
        assert_eq!(out, vec![Vec::<f32>::new(), Vec::<f32>::new()]);
    }
}
