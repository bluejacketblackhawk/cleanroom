//! Multi-resolution min/max peaks pyramid for waveform drawing (ADR-010).
//!
//! The UI never sees raw PCM. Instead it asks for `target_bins` (min, max) pairs over an
//! arbitrary `[start, end)` frame window, and gets them in ~O(bins) regardless of file
//! length or zoom. We pre-fold the samples into a pyramid of min/max levels; a query
//! picks the coarsest level fine enough for the requested resolution and folds a small,
//! bounded number of its bins per output bin.
//!
//! **Channel handling:** every channel is folded into a single min/max envelope — the
//! output min is the minimum sample across *all* channels in the bin, the max the maximum
//! across all channels. That draws one waveform outline that never clips off a transient
//! on either channel. (Per-channel lanes can be added later by keeping a pyramid per
//! channel; M0 draws one.)

use anvil_media::AudioBuffer;

/// Samples per bin at the finest pyramid level. Coarser levels multiply by
/// [`LEVEL_FACTOR`] (256, 1024, 4096, …), matching ADR-010's "min/max peaks at multiple
/// zoom levels".
const BASE_BIN: usize = 256;
/// Fan-in from one level to the next coarser one.
const LEVEL_FACTOR: usize = 4;

/// A pre-built pyramid of min/max bins at geometrically growing resolutions.
///
/// Build once per loaded file (cheap: linear in sample count — a 1-hour file is well
/// under a second), then slice it for every zoom/scroll without touching raw samples
/// again.
#[derive(Debug, Clone)]
pub struct PeaksPyramid {
    /// `levels[0]` is the finest ([`BASE_BIN`] frames per bin); each subsequent level is
    /// [`LEVEL_FACTOR`]× coarser. Each entry is `(min, max)` over its frame span.
    levels: Vec<Vec<(f32, f32)>>,
    /// Total source frames the pyramid was built from (length of one channel).
    frames: usize,
}

impl PeaksPyramid {
    /// Fold an [`AudioBuffer`] into the pyramid. Linear in the sample count.
    ///
    /// The finest level is built directly from the samples (one pass, all channels); each
    /// coarser level folds groups of [`LEVEL_FACTOR`] bins from the level below, so total
    /// work is `n + n/4 + n/16 + … < 1.34 n`.
    pub fn build(buffer: &AudioBuffer) -> Self {
        let frames = buffer.frames();
        if frames == 0 {
            return Self {
                levels: Vec::new(),
                frames: 0,
            };
        }

        // Finest level: min/max over each BASE_BIN window across every channel.
        let n_base = frames.div_ceil(BASE_BIN);
        let mut base = Vec::with_capacity(n_base);
        for bin in 0..n_base {
            let start = bin * BASE_BIN;
            let end = (start + BASE_BIN).min(frames);
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for ch in buffer.planar() {
                for &s in &ch[start..end] {
                    lo = lo.min(s);
                    hi = hi.max(s);
                }
            }
            base.push((lo, hi));
        }

        let mut levels = vec![base];
        // Coarser levels until a level holds a single bin (no point going coarser).
        while levels.last().map_or(0, Vec::len) > 1 {
            let prev = levels.last().unwrap();
            let n = prev.len().div_ceil(LEVEL_FACTOR);
            let mut next = Vec::with_capacity(n);
            for bin in 0..n {
                let start = bin * LEVEL_FACTOR;
                let end = (start + LEVEL_FACTOR).min(prev.len());
                let mut lo = f32::INFINITY;
                let mut hi = f32::NEG_INFINITY;
                for &(l, h) in &prev[start..end] {
                    lo = lo.min(l);
                    hi = hi.max(h);
                }
                next.push((lo, hi));
            }
            levels.push(next);
        }

        Self { levels, frames }
    }

    /// Total source frames represented (length of a channel).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Frames per bin at pyramid `level` (0 = finest).
    fn level_bin_size(level: usize) -> usize {
        BASE_BIN * LEVEL_FACTOR.pow(level as u32)
    }

    /// `(min, max)` pairs for `target_bins` evenly-spaced windows spanning
    /// `[start_frame, end_frame)`. Clamped to the loaded range.
    ///
    /// Runs in ~O(`target_bins`): it selects the coarsest level whose bin size is no
    /// larger than the per-output-bin span, so each output bin folds fewer than
    /// [`LEVEL_FACTOR`] source bins. Windows with no samples come back as `(0.0, 0.0)`.
    pub fn peaks(
        &self,
        start_frame: usize,
        end_frame: usize,
        target_bins: usize,
    ) -> Vec<(f32, f32)> {
        if self.levels.is_empty() || target_bins == 0 {
            return Vec::new();
        }
        let start = start_frame.min(self.frames);
        let end = end_frame.min(self.frames);
        if end <= start {
            return Vec::new();
        }
        let span = end - start;

        // Frames we'd like each output bin to cover, and the coarsest level no finer than
        // that. Falls back to the finest level when zoomed in past base resolution.
        let frames_per_bin = (span / target_bins).max(1);
        let mut level = 0;
        while level + 1 < self.levels.len() && Self::level_bin_size(level + 1) <= frames_per_bin {
            level += 1;
        }
        let bins = &self.levels[level];
        let bin_size = Self::level_bin_size(level);

        let mut out = Vec::with_capacity(target_bins);
        for i in 0..target_bins {
            // Frame window for this output bin (integer math keeps bins gapless).
            let w_start = start + (span * i) / target_bins;
            let w_end = start + (span * (i + 1)) / target_bins;
            let w_end = w_end.max(w_start + 1).min(end);

            // Source-level bins overlapping [w_start, w_end).
            let lb_start = w_start / bin_size;
            let lb_end = (w_end.div_ceil(bin_size)).min(bins.len());

            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for &(l, h) in &bins[lb_start..lb_end.max(lb_start + 1).min(bins.len())] {
                lo = lo.min(l);
                hi = hi.max(h);
            }
            if lo.is_finite() && hi.is_finite() {
                out.push((lo, hi));
            } else {
                out.push((0.0, 0.0));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-channel buffer that is `+val` for the first `split` frames then `-val`.
    fn two_step(split: usize, total: usize, val: f32) -> AudioBuffer {
        let ch: Vec<f32> = (0..total)
            .map(|i| if i < split { val } else { -val })
            .collect();
        AudioBuffer::from_planar(vec![ch], 48_000)
    }

    #[test]
    fn empty_buffer_yields_no_peaks() {
        let buf = AudioBuffer::from_planar(vec![Vec::new()], 48_000);
        let p = PeaksPyramid::build(&buf);
        assert_eq!(p.frames(), 0);
        assert!(p.peaks(0, 100, 8).is_empty());
    }

    #[test]
    fn constant_signal_min_equals_max() {
        let buf = AudioBuffer::from_planar(vec![vec![0.5; 1024]], 48_000);
        let p = PeaksPyramid::build(&buf);
        let got = p.peaks(0, 1024, 4);
        assert_eq!(got.len(), 4);
        for (lo, hi) in got {
            assert!((lo - 0.5).abs() < 1e-6, "min was {lo}");
            assert!((hi - 0.5).abs() < 1e-6, "max was {hi}");
        }
    }

    #[test]
    fn two_bins_capture_each_half() {
        // 512 frames = exactly two BASE_BIN bins: first +1, second -1.
        let buf = two_step(BASE_BIN, BASE_BIN * 2, 1.0);
        let p = PeaksPyramid::build(&buf);
        let got = p.peaks(0, BASE_BIN * 2, 2);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (1.0, 1.0)); // first half all +1
        assert_eq!(got[1], (-1.0, -1.0)); // second half all -1
    }

    #[test]
    fn single_bin_over_split_sees_full_swing() {
        // One output bin spanning a +1/-1 signal must report the whole range.
        let buf = two_step(BASE_BIN, BASE_BIN * 2, 1.0);
        let p = PeaksPyramid::build(&buf);
        let got = p.peaks(0, BASE_BIN * 2, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], (-1.0, 1.0));
    }

    #[test]
    fn folds_channels_into_one_envelope() {
        // ch0 = +1 constant, ch1 = -1 constant → envelope is (-1, +1) everywhere.
        let ch0 = vec![1.0f32; 1024];
        let ch1 = vec![-1.0f32; 1024];
        let buf = AudioBuffer::from_planar(vec![ch0, ch1], 48_000);
        let p = PeaksPyramid::build(&buf);
        for (lo, hi) in p.peaks(0, 1024, 4) {
            assert_eq!((lo, hi), (-1.0, 1.0));
        }
    }

    #[test]
    fn coarse_level_used_for_large_span_still_correct() {
        // A ramp of distinct bins; a low target_bins forces a coarser level, whose folded
        // min/max must still bound the true extremes.
        let total = BASE_BIN * 64;
        let ch: Vec<f32> = (0..total).map(|i| (i as f32) / (total as f32)).collect();
        let buf = AudioBuffer::from_planar(vec![ch], 48_000);
        let p = PeaksPyramid::build(&buf);
        let got = p.peaks(0, total, 4);
        assert_eq!(got.len(), 4);
        // Monotone ramp: each quarter's max ≈ its right edge, min ≈ its left edge.
        assert!(got[0].0 <= got[0].1);
        assert!((got[0].0 - 0.0).abs() < 0.02);
        assert!((got[3].1 - (total as f32 - 1.0) / total as f32).abs() < 0.02);
        // Ranges increase across the ramp.
        assert!(got[3].1 > got[0].1);
    }

    #[test]
    fn pyramid_has_multiple_levels() {
        // Enough frames to force several coarsening steps.
        let buf = AudioBuffer::from_planar(
            vec![vec![0.0; BASE_BIN * LEVEL_FACTOR * LEVEL_FACTOR]],
            48_000,
        );
        let p = PeaksPyramid::build(&buf);
        assert!(
            p.levels.len() >= 3,
            "expected ≥3 levels, got {}",
            p.levels.len()
        );
    }

    #[test]
    fn out_of_range_query_clamps() {
        let buf = AudioBuffer::from_planar(vec![vec![0.3; 500]], 48_000);
        let p = PeaksPyramid::build(&buf);
        // Query extends past the end; clamps to available frames.
        let got = p.peaks(400, 100_000, 4);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn one_hour_file_builds_quickly() {
        // Perf smoke: 1 hour mono @ 48 kHz. The algorithm is linear; this must be well
        // under the 5 s M0 budget even in a debug test build.
        let frames = 48_000 * 60 * 60;
        let buf = AudioBuffer::from_planar(vec![vec![0.1; frames]], 48_000);
        let t = std::time::Instant::now();
        let p = PeaksPyramid::build(&buf);
        let elapsed = t.elapsed();
        assert_eq!(p.frames(), frames);
        assert!(elapsed.as_secs() < 5, "pyramid build took {elapsed:?}");
    }
}
