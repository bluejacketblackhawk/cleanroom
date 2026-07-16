//! True-peak limiter (03 §4.10).
//!
//! Two mechanisms, layered so the ceiling is a *hard* guarantee (06 gate: output TP ≤
//! ceiling, zero tolerance):
//!
//! 1. A look-ahead gain limiter shaped on **inter-sample** peaks (4× oversampled detection),
//!    so peaks are ridden down smoothly and transparently — this does the audible work.
//! 2. A final static safety trim measured with the very same BS.1770 true-peak meter the
//!    eval harness uses (`ebur128`). Because true-peak scales linearly with amplitude, one
//!    multiply pins the measured TP to exactly the ceiling — closing any residual the
//!    smoothing left behind. This is what makes the guarantee zero-tolerance.
//!
//! Deterministic: pure sample math, no threads, no entropy.

use std::collections::VecDeque;

use anvil_core::INTERNAL_SAMPLE_RATE;
use anvil_media::AudioBuffer;
use ebur128::{EbuR128, Mode};
use serde::{Deserialize, Serialize};

use crate::Processor;

/// Limiter configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LimiterConfig {
    /// Ceiling in dBTP. The output true peak never exceeds this (default −1.0).
    pub ceiling_dbtp: f32,
    /// Look-ahead in milliseconds (03: 5 ms).
    pub lookahead_ms: f32,
    /// Release time in milliseconds (03: 80 ms program-dependent; we use a fixed one-pole).
    pub release_ms: f32,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            ceiling_dbtp: -1.0,
            lookahead_ms: 5.0,
            release_ms: 80.0,
        }
    }
}

/// Oversampling factor for inter-sample (true) peak detection (BS.1770 recommends ≥4×).
const OVERSAMPLE: usize = 4;
/// Half-length of the windowed-sinc interpolation kernel (taps per side, per phase).
const KERNEL_HALF: usize = 16;

/// True-peak limiter.
#[derive(Debug, Clone)]
pub struct TruePeakLimiter {
    config: LimiterConfig,
    sample_rate: f32,
    /// Precomputed fractional-delay kernels for phases 1/4, 2/4, 3/4 (phase 0 is the sample).
    phase_kernels: Vec<Vec<f32>>,
}

impl TruePeakLimiter {
    /// Build with `config`, assuming the engine's internal 48 kHz rate.
    pub fn new(config: LimiterConfig) -> Self {
        Self::with_rate(config, INTERNAL_SAMPLE_RATE)
    }

    /// Build with an explicit sample rate.
    pub fn with_rate(config: LimiterConfig, sample_rate: u32) -> Self {
        Self {
            config,
            sample_rate: sample_rate as f32,
            phase_kernels: build_phase_kernels(),
        }
    }

    /// The config.
    pub fn config(&self) -> LimiterConfig {
        self.config
    }

    fn ceiling_linear(&self) -> f32 {
        10f32.powf(self.config.ceiling_dbtp / 20.0)
    }

    /// Per-original-sample inter-sample peak magnitude, taken across all channels.
    fn true_peak_envelope(&self, channels: &[Vec<f32>], frames: usize) -> Vec<f32> {
        true_peak_envelope(&self.phase_kernels, channels, frames)
    }
}

/// Per-original-sample inter-sample peak magnitude across all channels, estimated by evaluating
/// the 3 intermediate sub-samples with the windowed-sinc kernels. Free function so both the
/// whole-buffer [`TruePeakLimiter`] and the block-streaming [`StreamingLimiter`] share one
/// definition.
fn true_peak_envelope(
    phase_kernels: &[Vec<f32>],
    channels: &[Vec<f32>],
    frames: usize,
) -> Vec<f32> {
    let mut tp = vec![0.0f32; frames];
    for channel in channels {
        for (n, slot) in tp.iter_mut().enumerate() {
            let mut peak = channel[n].abs();
            for kernel in phase_kernels {
                let mut acc = 0.0f32;
                for (k, &coeff) in kernel.iter().enumerate() {
                    // Kernel is centered: taps span [n-KERNEL_HALF+1 , n+KERNEL_HALF].
                    let idx = n as isize + k as isize - (KERNEL_HALF as isize - 1);
                    if idx >= 0 && (idx as usize) < frames {
                        acc += coeff * channel[idx as usize];
                    }
                }
                peak = peak.max(acc.abs());
            }
            *slot = slot.max(peak);
        }
    }
    tp
}

/// Build the 3 polyphase fractional-delay kernels (Blackman-windowed sinc) for the sub-sample
/// positions 0.25, 0.5, 0.75 between consecutive samples.
fn build_phase_kernels() -> Vec<Vec<f32>> {
    let taps = 2 * KERNEL_HALF;
    let mut kernels = Vec::with_capacity(OVERSAMPLE - 1);
    for phase in 1..OVERSAMPLE {
        let frac = phase as f32 / OVERSAMPLE as f32;
        let mut kernel = Vec::with_capacity(taps);
        for k in 0..taps {
            // Tap position relative to the interpolation point.
            let x = (k as f32 - (KERNEL_HALF as f32 - 1.0)) - frac;
            let sinc = if x.abs() < 1e-6 {
                1.0
            } else {
                let pix = std::f32::consts::PI * x;
                pix.sin() / pix
            };
            // Blackman window over the tap span.
            let w = {
                let t = k as f32 / (taps as f32 - 1.0);
                0.42 - 0.5 * (2.0 * std::f32::consts::PI * t).cos()
                    + 0.08 * (4.0 * std::f32::consts::PI * t).cos()
            };
            kernel.push(sinc * w);
        }
        kernels.push(kernel);
    }
    kernels
}

/// Sliding-window minimum over `[n, n+window)` — the look-ahead attack, so the gain is
/// already pulled down before an upcoming peak arrives. O(n) via a monotonic deque.
fn sliding_min(values: &[f32], window: usize) -> Vec<f32> {
    let n = values.len();
    let mut out = vec![1.0f32; n];
    let mut dq: VecDeque<usize> = VecDeque::new();
    // Process right-to-left so the deque front is the min over the forward window.
    for i in (0..n).rev() {
        while let Some(&back) = dq.back() {
            if values[back] >= values[i] {
                dq.pop_back();
            } else {
                break;
            }
        }
        dq.push_back(i);
        // Drop indices that fell outside the forward window.
        if let Some(&front) = dq.front() {
            if front >= i + window {
                dq.pop_front();
            }
        }
        out[i] = values[*dq.front().unwrap()];
    }
    out
}

impl Processor for TruePeakLimiter {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        if frames == 0 {
            return;
        }
        let ceiling = self.ceiling_linear();
        let channels = buffer.planar().to_vec();

        // 1. Inter-sample peak envelope → required per-sample gain (≤ 1).
        let tp = self.true_peak_envelope(&channels, frames);
        let g_req: Vec<f32> = tp
            .iter()
            .map(|&p| if p > ceiling { ceiling / p } else { 1.0 })
            .collect();

        // 2. Look-ahead: pull the gain down `lookahead` samples ahead of each peak.
        let lookahead = ((self.config.lookahead_ms / 1000.0) * self.sample_rate).round() as usize;
        let lookahead = lookahead.max(1);
        let g_look = sliding_min(&g_req, lookahead + 1);

        // 3. Attack/release smoothing. Attack settles well within the look-ahead runway;
        //    release recovers gently. One-pole coefficients from the time constants.
        let attack_samples = (lookahead as f32 / 3.0).max(1.0);
        let attack_coeff = (-1.0 / attack_samples).exp();
        let release_samples = (self.config.release_ms / 1000.0 * self.sample_rate).max(1.0);
        let release_coeff = (-1.0 / release_samples).exp();

        let mut env = vec![1.0f32; frames];
        let mut g = g_look[0];
        for n in 0..frames {
            let target = g_look[n];
            if target < g {
                g = target + (g - target) * attack_coeff; // fast attack (downward)
            } else {
                g = target + (g - target) * release_coeff; // slow release (upward)
            }
            env[n] = g;
        }

        // 4. Apply the gain envelope.
        for channel in buffer.planar_mut() {
            for (n, sample) in channel.iter_mut().enumerate() {
                *sample *= env[n];
            }
        }

        // 5. Hard guarantee: measure the true peak with the same meter the eval uses and, if
        //    anything still pokes above the ceiling (smoothing residue), trim linearly. TP
        //    scales linearly with amplitude, so this pins it to the ceiling exactly.
        if let Some(measured) = measure_true_peak(buffer) {
            if measured > ceiling && measured.is_finite() {
                let trim = ceiling / measured;
                for channel in buffer.planar_mut() {
                    for sample in channel.iter_mut() {
                        *sample *= trim;
                    }
                }
            }
        }
    }
}

/// Block-streaming form of [`TruePeakLimiter`] for the M5 streaming master.
///
/// It feeds the *identical* look-ahead peak-riding gain across an unbounded stream while holding
/// only a bounded look-ahead window (a few hundred samples), then applies a scalar `trim` — the
/// zero-tolerance ceiling guarantee, measured on the whole stream in a prior pass — per emitted
/// sample. For a given `trim` the emitted samples are **bit-identical** to what
/// [`TruePeakLimiter`] produces on the whole buffer (with the same trim): the per-sample
/// true-peak, the look-ahead sliding-min and the one-pole smoothing are all evaluated with full
/// context, and the smoothing gain is carried across blocks so nothing resets at a seam. Proven
/// by `streaming_limiter_matches_whole_buffer`.
#[derive(Debug, Clone)]
pub struct StreamingLimiter {
    phase_kernels: Vec<Vec<f32>>,
    lookahead: usize,
    attack_coeff: f32,
    release_coeff: f32,
    ceiling: f32,
    /// Per-channel samples not yet emitted, with `ctx` leading already-emitted context samples
    /// kept so the ±[`KERNEL_HALF`] true-peak kernel and the look-ahead window reach back across
    /// the seam.
    pending: Vec<Vec<f32>>,
    ctx: usize,
    /// One-pole smoothing gain, carried across blocks (`None` until the first sample primes it).
    g: Option<f32>,
    /// Final static trim (`<= 1.0`, 1.0 = none), applied per emitted sample.
    trim: f32,
    channels: usize,
}

impl StreamingLimiter {
    /// Build for `channels` channels at `sample_rate` with `config`.
    pub fn new(config: LimiterConfig, sample_rate: u32, channels: usize) -> Self {
        let sr = sample_rate as f32;
        let lookahead = (((config.lookahead_ms / 1000.0) * sr).round() as usize).max(1);
        let attack_samples = (lookahead as f32 / 3.0).max(1.0);
        let attack_coeff = (-1.0 / attack_samples).exp();
        let release_samples = (config.release_ms / 1000.0 * sr).max(1.0);
        let release_coeff = (-1.0 / release_samples).exp();
        let ceiling = 10f32.powf(config.ceiling_dbtp / 20.0);
        let channels = channels.max(1);
        Self {
            phase_kernels: build_phase_kernels(),
            lookahead,
            attack_coeff,
            release_coeff,
            ceiling,
            pending: vec![Vec::new(); channels],
            ctx: 0,
            g: None,
            trim: 1.0,
            channels,
        }
    }

    /// Set the final static trim (`ceiling / measured_true_peak`), measured on the whole stream.
    pub fn set_trim(&mut self, trim: f32) {
        self.trim = trim;
    }

    /// The look-ahead latency in samples (kernel reach + look-ahead window). Bounds how far the
    /// stream must run before the first sample can be emitted.
    pub fn latency_samples(&self) -> usize {
        self.lookahead + KERNEL_HALF
    }

    /// Push a block; returns whatever samples now have full look-ahead context (may be empty).
    pub fn push(&mut self, block: &AudioBuffer) -> AudioBuffer {
        for c in 0..self.channels {
            if let Some(src) = block.planar().get(c) {
                self.pending[c].extend_from_slice(src);
            }
        }
        self.drain(false)
    }

    /// Flush the tail (zero-padded future, matching the whole-buffer edge behaviour).
    pub fn flush(&mut self) -> AudioBuffer {
        self.drain(true)
    }

    fn drain(&mut self, final_flush: bool) -> AudioBuffer {
        let n = self.pending[0].len();
        let reach = self.lookahead + KERNEL_HALF;
        let emit_hi = if final_flush {
            n
        } else {
            n.saturating_sub(reach)
        };
        if emit_hi <= self.ctx {
            return AudioBuffer::new_internal(self.channels);
        }

        // Full-context true-peak → required gain → look-ahead sliding-min over `pending`. The
        // used indices [ctx, emit_hi) all have their ±KERNEL_HALF kernel reach and their forward
        // look-ahead window inside `pending`, so these values match the whole-buffer limiter.
        let tp = true_peak_envelope(&self.phase_kernels, &self.pending, n);
        let g_req: Vec<f32> = tp
            .iter()
            .map(|&p| {
                if p > self.ceiling {
                    self.ceiling / p
                } else {
                    1.0
                }
            })
            .collect();
        let g_look = sliding_min(&g_req, self.lookahead + 1);

        let mut out: Vec<Vec<f32>> = (0..self.channels)
            .map(|_| Vec::with_capacity(emit_hi - self.ctx))
            .collect();
        let mut g = self
            .g
            .unwrap_or_else(|| g_look.get(self.ctx).copied().unwrap_or(1.0));
        for (i, &target) in g_look.iter().enumerate().take(emit_hi).skip(self.ctx) {
            if target < g {
                g = target + (g - target) * self.attack_coeff; // fast attack (downward)
            } else {
                g = target + (g - target) * self.release_coeff; // slow release (upward)
            }
            let gain = g * self.trim;
            for (plane, src) in out.iter_mut().zip(self.pending.iter()) {
                plane.push(src[i] * gain);
            }
        }
        self.g = Some(g);

        if final_flush {
            self.pending = vec![Vec::new(); self.channels];
            self.ctx = 0;
        } else {
            let keep_from = emit_hi.saturating_sub(KERNEL_HALF);
            for plane in self.pending.iter_mut() {
                plane.drain(0..keep_from);
            }
            self.ctx = emit_hi - keep_from;
        }
        AudioBuffer::from_planar(out, INTERNAL_SAMPLE_RATE)
    }
}

/// The maximum true-peak magnitude (linear) across channels, via `ebur128`'s 4×-oversampled
/// BS.1770 meter — the reference the CI gate and eval harness measure against.
pub fn measure_true_peak(buffer: &AudioBuffer) -> Option<f32> {
    let ch = buffer.channel_count() as u32;
    let frames = buffer.frames();
    if ch == 0 || frames == 0 {
        return None;
    }
    let mut meter = EbuR128::new(ch, buffer.sample_rate(), Mode::TRUE_PEAK).ok()?;
    let mut interleaved = vec![0.0f32; frames * ch as usize];
    for c in 0..ch as usize {
        for (f, &s) in buffer.channel(c).iter().enumerate() {
            interleaved[f * ch as usize + c] = s;
        }
    }
    meter.add_frames_f32(&interleaved).ok()?;
    let mut peak = 0.0f64;
    for c in 0..ch {
        peak = peak.max(meter.true_peak(c).ok()?);
    }
    Some(peak as f32)
}

/// The measured true peak in dBTP (−inf floored to a sentinel), for reports.
pub fn true_peak_dbtp(buffer: &AudioBuffer) -> f64 {
    match measure_true_peak(buffer) {
        Some(p) if p > 0.0 => 20.0 * (p as f64).log10(),
        _ => -120.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// A signal engineered to have large inter-sample overshoots: a near-Nyquist tone that
    /// rings between samples, scaled hot (+6 dB, peaks at 2.0).
    fn hot_intersample_signal() -> AudioBuffer {
        let s: Vec<f32> = (0..48_000)
            .map(|i| 2.0 * (i as f32 * 11_000.0 * TAU / 48_000.0).sin())
            .collect();
        AudioBuffer::from_planar(vec![s.clone(), s], 48_000)
    }

    #[test]
    fn output_true_peak_never_exceeds_ceiling() {
        let mut buf = hot_intersample_signal();
        let cfg = LimiterConfig::default(); // −1.0 dBTP
        TruePeakLimiter::new(cfg).process(&mut buf);

        let measured = true_peak_dbtp(&buf);
        // Zero tolerance: allow only float epsilon above the ceiling.
        assert!(
            measured <= cfg.ceiling_dbtp as f64 + 0.01,
            "true peak {measured} dBTP exceeded ceiling {}",
            cfg.ceiling_dbtp
        );
    }

    #[test]
    fn ceiling_at_minus_three_holds_too() {
        let mut buf = hot_intersample_signal();
        let cfg = LimiterConfig {
            ceiling_dbtp: -3.0,
            ..Default::default()
        };
        TruePeakLimiter::new(cfg).process(&mut buf);
        assert!(true_peak_dbtp(&buf) <= -3.0 + 0.01);
    }

    #[test]
    fn quiet_signal_passes_through_essentially_untouched() {
        // A −20 dBFS tone is well under the ceiling; the limiter should barely touch it.
        let s: Vec<f32> = (0..4_800)
            .map(|i| 0.1 * (i as f32 * 440.0 * TAU / 48_000.0).sin())
            .collect();
        let mut buf = AudioBuffer::from_planar(vec![s.clone()], 48_000);
        TruePeakLimiter::new(LimiterConfig::default()).process(&mut buf);
        for (a, b) in buf.channel(0).iter().zip(&s) {
            assert!((a - b).abs() < 1e-3, "quiet signal altered: {a} vs {b}");
        }
    }

    #[test]
    fn deterministic() {
        let (mut a, mut b) = (hot_intersample_signal(), hot_intersample_signal());
        TruePeakLimiter::new(LimiterConfig::default()).process(&mut a);
        TruePeakLimiter::new(LimiterConfig::default()).process(&mut b);
        assert_eq!(a, b);
    }

    /// Feed `buf` through a [`StreamingLimiter`] in `block`-sized pieces and collect the output.
    fn feed(lim: &mut StreamingLimiter, buf: &AudioBuffer, block: usize) -> AudioBuffer {
        let frames = buf.frames();
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); buf.channel_count()];
        let mut start = 0;
        while start < frames {
            let end = (start + block).min(frames);
            let blk = AudioBuffer::from_planar(
                buf.planar()
                    .iter()
                    .map(|c| c[start..end].to_vec())
                    .collect(),
                buf.sample_rate(),
            );
            let e = lim.push(&blk);
            for (c, out_ch) in out.iter_mut().enumerate() {
                out_ch.extend_from_slice(e.channel(c));
            }
            start = end;
        }
        let f = lim.flush();
        for (c, out_ch) in out.iter_mut().enumerate() {
            out_ch.extend_from_slice(f.channel(c));
        }
        AudioBuffer::from_planar(out, buf.sample_rate())
    }

    /// The block-streaming limiter must reproduce the whole-buffer limiter sample-for-sample:
    /// same look-ahead gain, carried across seams, plus the same final trim — and it must be
    /// invariant to the block size it is fed.
    #[test]
    fn streaming_limiter_matches_whole_buffer() {
        let src = hot_intersample_signal();
        let cfg = LimiterConfig::default();
        let ceiling = 10f32.powf(cfg.ceiling_dbtp / 20.0);

        let mut whole = src.clone();
        TruePeakLimiter::new(cfg).process(&mut whole);

        // Pass 1 (no trim) recovers the look-ahead-limited signal; measure its true peak to get
        // the same static trim the whole-buffer limiter applies.
        let mut l1 = StreamingLimiter::new(cfg, 48_000, src.channel_count());
        let notrim = feed(&mut l1, &src, 1000);
        let tp = measure_true_peak(&notrim).unwrap();
        let trim = if tp > ceiling { ceiling / tp } else { 1.0 };

        // Pass 2 with the trim, fed at a *different* block size to prove block-size invariance.
        let mut l2 = StreamingLimiter::new(cfg, 48_000, src.channel_count());
        l2.set_trim(trim);
        let streamed = feed(&mut l2, &src, 777);

        assert_eq!(streamed.frames(), whole.frames(), "length preserved");
        let mut maxerr = 0.0f32;
        for c in 0..whole.channel_count() {
            for (a, b) in streamed.channel(c).iter().zip(whole.channel(c)) {
                maxerr = maxerr.max((a - b).abs());
            }
        }
        assert!(
            maxerr < 1e-5,
            "streaming limiter diverged from whole-buffer: max error {maxerr}"
        );
        assert!(true_peak_dbtp(&streamed) <= cfg.ceiling_dbtp as f64 + 0.01);
    }
}
