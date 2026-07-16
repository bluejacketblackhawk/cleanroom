//! Silence cutting (03 §5): VAD-negative runs `≥ min_gap` are shortened to `target_gap` with
//! the removed span taken from the middle of the run, so both crossfade edges land in
//! near-silence. Runs inside music segments or straddling a protected chapter boundary are
//! left alone; the kept gap never drops below `silence_floor`.
//!
//! The silence runs themselves come from the analysis pass (`anvil_dsp` `silence_runs`) in
//! production; [`detect_silence`] is an in-crate energy VAD so this crate needs no `anvil-dsp`
//! dependency and stays testable standalone.

use anvil_core::{HOP_SAMPLES, INTERNAL_SAMPLE_RATE};
use anvil_media::AudioBuffer;

use crate::{Cut, CutKind, CutOptions, SilenceInput, TimeRange};

/// Turn qualifying silence runs into [`Cut`]s (03 §5).
pub(crate) fn plan_silence_cuts(input: &SilenceInput, opts: &CutOptions) -> Vec<Cut> {
    // Never leave an unnaturally short gap: the kept gap is at least `silence_floor` (03 §5:
    // "never cut below 0.4 s").
    let target_gap = opts.target_gap.max(opts.silence_floor);

    input
        .runs
        .iter()
        .filter_map(|run| {
            let dur = run.duration();
            // Only shorten runs that are both long enough to bother and long enough that the
            // kept `target_gap` genuinely shortens them.
            if dur < opts.min_gap || dur <= target_gap {
                return None;
            }
            if input.music.iter().any(|m| m.overlaps(run)) {
                return None; // 03 §5: never inside music segments.
            }
            if input
                .protected_times
                .iter()
                .any(|&t| run.contains(t) || t == run.start || t == run.end)
            {
                return None; // 03 §5: chapter-boundary silences protected.
            }

            // Keep `target_gap` total, split evenly across the run's ends, and remove the
            // middle. Both cut edges then sit inside silence, so the equal-power crossfade at
            // render joins two low-energy samples (small seam step — 06 §2 cut-artifact gate).
            let keep_each = target_gap / 2.0;
            let cut_start = run.start + keep_each;
            let cut_end = run.end - keep_each;
            Some(Cut {
                start: cut_start,
                end: cut_end,
                kind: CutKind::Silence,
                label: format!("silence {dur:.1}s→{target_gap:.1}s"),
                accepted: true,
            })
        })
        .collect()
}

/// Default RMS gate for the in-crate VAD: −50 dBFS. Below this a hop is treated as silence
/// unless the file's own noise floor sits higher, in which case the adaptive term takes over.
const SILENCE_FLOOR_DBFS: f32 = -50.0;
/// Minimum run length recorded, matching the analysis "silence map" (03 §1: runs > 300 ms).
const MIN_RUN_SECS: f64 = 0.3;

/// A simple, deterministic energy VAD (03 §1 "VAD" fallback). Frames the buffer into 10 ms
/// hops, marks hops whose RMS is below an adaptive gate as non-speech, and returns coalesced
/// runs `> MIN_RUN_SECS`. This is the standalone/in-crate source of silence runs; the
/// production path uses `anvil_dsp` `silence_runs` instead (both feed [`crate::plan`]
/// identically). Documented as a fallback — it is intentionally cheap, not Silero-grade.
pub fn detect_silence(buffer: &AudioBuffer) -> Vec<TimeRange> {
    let sr = if buffer.sample_rate() == 0 {
        INTERNAL_SAMPLE_RATE
    } else {
        buffer.sample_rate()
    };
    let frames = buffer.frames();
    if frames == 0 {
        return Vec::new();
    }

    // Per-hop RMS across all channels.
    let hop = HOP_SAMPLES;
    let n_hops = frames.div_ceil(hop);
    let mut rms = vec![0.0_f32; n_hops];
    for (h, slot) in rms.iter_mut().enumerate() {
        let start = h * hop;
        let end = (start + hop).min(frames);
        let len = (end - start).max(1);
        let mut sum_sq = 0.0_f64;
        for ch in buffer.planar() {
            for &s in &ch[start..end] {
                sum_sq += f64::from(s) * f64::from(s);
            }
        }
        let denom = (len * buffer.channel_count().max(1)) as f64;
        *slot = (sum_sq / denom).sqrt() as f32;
    }

    // Adaptive gate: max of the fixed floor and a small margin over the 10th-percentile hop
    // (the file's own noise floor), so a hissy recording still resolves gaps.
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p10 = sorted[sorted.len() / 10];
    let fixed = 10f32.powf(SILENCE_FLOOR_DBFS / 20.0);
    let gate = fixed.max(p10 * 3.0);

    let hop_secs = hop as f64 / f64::from(sr);
    let min_hops = (MIN_RUN_SECS / hop_secs).ceil() as usize;

    let mut runs = Vec::new();
    let mut run_start: Option<usize> = None;
    let close = |runs: &mut Vec<TimeRange>, start: usize, end: usize| {
        if end - start >= min_hops {
            runs.push(TimeRange::new(
                start as f64 * hop_secs,
                end as f64 * hop_secs,
            ));
        }
    };
    for (h, &r) in rms.iter().enumerate() {
        if r < gate {
            run_start.get_or_insert(h);
        } else if let Some(s) = run_start.take() {
            close(&mut runs, s, h);
        }
    }
    // Close a run that reaches the end of the file.
    if let Some(s) = run_start.take() {
        close(&mut runs, s, rms.len());
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_run_is_not_cut() {
        // 1.0 s run is below the 1.5 s default min_gap.
        let input = SilenceInput::from_runs([(2.0, 3.0)]);
        assert!(plan_silence_cuts(&input, &CutOptions::default()).is_empty());
    }

    #[test]
    fn long_run_shortens_to_target_centered() {
        let input = SilenceInput::from_runs([(10.0, 13.0)]);
        let cuts = plan_silence_cuts(&input, &CutOptions::default());
        assert_eq!(cuts.len(), 1);
        // Keep 0.35 s at each end of the 3 s run → cut 10.35..12.65.
        assert!((cuts[0].start - 10.35).abs() < 1e-9);
        assert!((cuts[0].end - 12.65).abs() < 1e-9);
        assert_eq!(cuts[0].kind, CutKind::Silence);
    }

    #[test]
    fn target_gap_never_below_floor() {
        let opts = CutOptions {
            target_gap: 0.1, // below the 0.4 s floor
            ..CutOptions::default()
        };
        let input = SilenceInput::from_runs([(0.0, 3.0)]);
        let cuts = plan_silence_cuts(&input, &opts);
        assert_eq!(cuts.len(), 1);
        // Kept gap is clamped up to the 0.4 s floor, not 0.1 s.
        let kept = 3.0 - cuts[0].duration();
        assert!((kept - 0.4).abs() < 1e-9, "kept {kept}");
    }

    #[test]
    fn detect_silence_finds_the_gap() {
        // 0.5 s tone, 1.0 s silence, 0.5 s tone at 48 kHz mono.
        let sr: usize = 48_000;
        let tone = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| 0.5 * (i as f32 * 300.0 * std::f32::consts::TAU / sr as f32).sin())
                .collect()
        };
        let mut samples = tone(sr / 2);
        samples.extend(vec![0.0_f32; sr]);
        samples.extend(tone(sr / 2));
        let buf = AudioBuffer::from_planar(vec![samples], sr as u32);

        let runs = detect_silence(&buf);
        assert_eq!(runs.len(), 1, "expected one silence run, got {runs:?}");
        // Gap is roughly 0.5..1.5 s.
        assert!(
            runs[0].start >= 0.45 && runs[0].start <= 0.6,
            "{:?}",
            runs[0]
        );
        assert!(runs[0].end >= 1.4 && runs[0].end <= 1.55, "{:?}", runs[0]);
    }
}
