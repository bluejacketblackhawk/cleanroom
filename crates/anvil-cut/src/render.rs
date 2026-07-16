//! Rendering an [`Edl`] to audio (03 §5: "applied at render"). Kept segments are copied out
//! of the source buffer and joined with an equal-power crossfade at every seam, so a cut
//! never leaves a click. The apply step is the only place audio is touched — the plan and EDL
//! are pure data.

use anvil_media::AudioBuffer;
use anvil_project::edl::{Edl, Segment};

/// Crossfade length at every cut join (03 §5: "60 ms equal-power crossfades").
pub const DEFAULT_CROSSFADE_SECS: f64 = 0.060;

/// Render `edl`'s kept segments against `buffer` with the default 60 ms crossfade (03 §5).
/// The EDL is single-source (index 0, produced by [`crate::to_edl`]); segment times are in
/// source seconds and clamped to the buffer.
pub fn apply(edl: &Edl, buffer: &AudioBuffer) -> AudioBuffer {
    apply_with_crossfade(edl, buffer, DEFAULT_CROSSFADE_SECS)
}

/// [`apply`] with an explicit crossfade length. The crossfade at any join is shrunk to fit
/// the shorter of the two adjoining kept segments, so back-to-back short cuts never overrun.
pub fn apply_with_crossfade(edl: &Edl, buffer: &AudioBuffer, crossfade_secs: f64) -> AudioBuffer {
    let sr = buffer.sample_rate();
    let frames = buffer.frames();
    let channels = buffer.channel_count();

    // Kept segments → clamped sample ranges, in timeline order.
    let ranges: Vec<(usize, usize)> = edl
        .kept_ranges()
        .map(|s| sample_range(s, sr, frames))
        .filter(|&(a, b)| b > a)
        .collect();

    if ranges.is_empty() || channels == 0 {
        return AudioBuffer::new(channels.max(1), if sr == 0 { 48_000 } else { sr });
    }

    let cf_full = (crossfade_secs * f64::from(sr)).round() as usize;

    // Per-join crossfade length, derived once from the ranges so every channel joins
    // identically (determinism + channel alignment).
    let joins: Vec<usize> = ranges
        .windows(2)
        .map(|w| {
            let prev_len = w[0].1 - w[0].0;
            let cur_len = w[1].1 - w[1].0;
            cf_full.min(prev_len).min(cur_len)
        })
        .collect();

    let out_channels: Vec<Vec<f32>> = (0..channels)
        .map(|ch| render_channel(buffer.channel(ch), &ranges, &joins))
        .collect();

    AudioBuffer::from_planar(out_channels, sr)
}

/// Fold one channel's kept ranges into a single track, crossfading each seam.
fn render_channel(samples: &[f32], ranges: &[(usize, usize)], joins: &[usize]) -> Vec<f32> {
    let total: usize =
        ranges.iter().map(|&(a, b)| b - a).sum::<usize>() - joins.iter().sum::<usize>();
    let mut out: Vec<f32> = Vec::with_capacity(total);

    // First segment verbatim.
    out.extend_from_slice(&samples[ranges[0].0..ranges[0].1]);

    for (idx, &(a, b)) in ranges.iter().enumerate().skip(1) {
        let xf = joins[idx - 1];
        let seg = &samples[a..b];
        // Equal-power crossfade: blend the outgoing tail (already in `out`) with the incoming
        // head. w_out² + w_in² = 1 keeps perceived energy constant across the seam.
        let tail_start = out.len() - xf;
        for (i, (o, &s)) in out[tail_start..].iter_mut().zip(&seg[..xf]).enumerate() {
            let (w_out, w_in) = equal_power_weights(i, xf);
            *o = *o * w_out + s * w_in;
        }
        out.extend_from_slice(&seg[xf..]);
    }
    out
}

/// Equal-power (sin/cos) crossfade weights at position `i` of a `len`-sample fade. Sampled at
/// bin centers so the fade is symmetric and never hits an exact 0×0 seam.
fn equal_power_weights(i: usize, len: usize) -> (f32, f32) {
    if len == 0 {
        return (1.0, 0.0);
    }
    let t = (i as f32 + 0.5) / len as f32; // 0..1
    let angle = t * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

/// Convert a kept [`Segment`]'s source seconds to a clamped `[in, out)` sample range.
fn sample_range(seg: &Segment, sr: u32, frames: usize) -> (usize, usize) {
    let to_sample = |t: f64| -> usize {
        if t <= 0.0 {
            0
        } else {
            ((t * f64::from(sr)).round() as usize).min(frames)
        }
    };
    let a = to_sample(seg.source_in);
    let b = to_sample(seg.source_out).max(a);
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plan, CutOptions, SilenceInput};
    use anvil_asr::Transcript;
    use std::f32::consts::TAU;

    /// A 300 Hz tone on `[0,1)` and `[3,4)` s with silence between, mono @ 48 kHz. The tone is
    /// phase-continuous at the silence boundaries (300 whole cycles), so the *input* has only
    /// small sample-to-sample steps.
    fn gapped_tone(sr: u32) -> AudioBuffer {
        let n = 4 * sr as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(sr);
                if !(1.0..3.0).contains(&t) {
                    0.5 * (t * 300.0 * f64::from(TAU)).sin() as f32
                } else {
                    0.0
                }
            })
            .collect();
        AudioBuffer::from_planar(vec![samples], sr)
    }

    fn max_abs_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// Cut-artifact gate (06 §2): after apply, the crossfade seam introduces no large sample
    /// discontinuity — the rendered max step stays within the input's natural range.
    #[test]
    fn crossfade_seam_has_no_large_discontinuity() {
        let sr = 48_000;
        let buf = gapped_tone(sr);
        let input_step = max_abs_step(buf.channel(0));

        // A 2 s silence run at 1.0..3.0 → cut the middle, keeping 0.35 s each side.
        let silence = SilenceInput::from_runs([(1.0, 3.0)]);
        let cut_plan = plan(
            &Transcript::default(),
            &[],
            &silence,
            &CutOptions::default(),
        )
        .with_source_duration(4.0);
        let edl = crate::to_edl(&cut_plan);
        let out = apply(&edl, &buf);

        let out_step = max_abs_step(out.channel(0));
        // The seam lands in near-silence, so no new step appears; allow a hair over the tone's
        // own step for rounding.
        assert!(
            out_step <= input_step + 0.01,
            "seam step {out_step} exceeds input {input_step}"
        );

        // Length shrank by exactly (cut span − one crossfade). Cut is 1.35..2.65 (1.30 s).
        let cut_len = ((2.65 - 1.35) * f64::from(sr)).round() as usize;
        let cf = (DEFAULT_CROSSFADE_SECS * f64::from(sr)).round() as usize;
        assert_eq!(
            out.frames(),
            buf.frames() - cut_len - cf,
            "unexpected rendered length"
        );
    }

    /// A no-cut EDL renders the source unchanged.
    #[test]
    fn empty_plan_is_identity() {
        let sr = 48_000;
        let buf = gapped_tone(sr);
        let cut_plan = plan(
            &Transcript::default(),
            &[],
            &SilenceInput::default(),
            &CutOptions::default(),
        )
        .with_source_duration(4.0);
        let edl = crate::to_edl(&cut_plan);
        let out = apply(&edl, &buf);
        assert_eq!(out.frames(), buf.frames());
        assert_eq!(out.channel(0), buf.channel(0));
    }

    /// Determinism (06 §2): double-render is bit-identical.
    #[test]
    fn apply_is_deterministic() {
        let sr = 48_000;
        let buf = gapped_tone(sr);
        let silence = SilenceInput::from_runs([(1.0, 3.0)]);
        let cut_plan = plan(
            &Transcript::default(),
            &[],
            &silence,
            &CutOptions::default(),
        )
        .with_source_duration(4.0);
        let edl = crate::to_edl(&cut_plan);
        let a = apply(&edl, &buf);
        let b = apply(&edl, &buf);
        assert_eq!(a, b);
    }

    /// Crossfade shortens output by one fade length per join (two kept segments = one join).
    #[test]
    fn crossfade_consumes_one_fade_per_join() {
        let sr = 48_000;
        // Simple DC-ish segments so lengths are easy to reason about.
        let samples = vec![1.0_f32; sr as usize]; // 1 s
        let buf = AudioBuffer::from_planar(vec![samples], sr);
        // Cut 0.4..0.6 (0.2 s) from the middle by hand.
        let edl = crate::to_edl(&crate::CutPlan {
            cuts: vec![crate::Cut {
                start: 0.4,
                end: 0.6,
                kind: crate::CutKind::Silence,
                label: "x".into(),
                accepted: true,
            }],
            source_duration: 1.0,
        });
        // Two kept segments: 0..0.4 and 0.6..1.0.
        assert_eq!(edl.kept_ranges().count(), 2);
        let out = apply(&edl, &buf);
        let kept = (0.4 + 0.4) * f64::from(sr);
        let cf = DEFAULT_CROSSFADE_SECS * f64::from(sr);
        assert_eq!(out.frames(), (kept - cf).round() as usize);
    }
}
