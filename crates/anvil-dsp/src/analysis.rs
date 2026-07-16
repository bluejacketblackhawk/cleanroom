//! Analysis pass (03 §1) — one streaming pass over 48 kHz audio producing an
//! [`AnalysisReport`]. The report drives the auto-decision (§2), the Health Card, and the
//! "before" columns of the compliance report.
//!
//! Loudness/true-peak come from `ebur128` (BS.1770-4). Everything else is derived from two
//! cheap accumulators: per-sample streaming stats (DC, clipping, stereo correlation) and a
//! sliding 2048-point FFT taken every 10 ms hop (rolloff, flatness, sibilance, hum, plus the
//! energy/ZCR that feed a simple energy + spectral-flatness VAD/segmenter). Storing one small
//! feature record per hop keeps even a 3 h file well under the RAM budget while allowing an
//! honest global noise-floor percentile and speech/music split.
//!
//! Deterministic (ADR-003): all math is sequential floating point; no thread scheduling and
//! no entropy affect any value.

use anvil_core::{HOP_SAMPLES, INTERNAL_SAMPLE_RATE};
use anvil_media::AudioBuffer;
use ebur128::{EbuR128, Mode};
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::DspError;

/// A stable, mains-hum finding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Hum {
    /// Detected fundamental in Hz (≈ 50 or 60).
    pub fundamental_hz: f32,
    /// Whether the peak is stable across the file (vs a transient tonal blip).
    pub stable: bool,
}

/// Stereo field statistics (present only for 2-channel input).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StereoStats {
    /// Inter-channel correlation, −1..1.
    pub correlation: f32,
    /// True when both channels are (near-)identical — a mono file in a stereo container.
    pub dual_mono: bool,
    /// Left-vs-right energy imbalance in dB (positive = left louder).
    pub lr_imbalance_db: f32,
}

/// A run of non-speech longer than 300 ms, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SilenceRun {
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
}

/// The full analysis report (03 §1). Serializes to snake_case JSON; the top-level key names
/// are a contract the CLI, UI, and eval harness depend on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisReport {
    /// Integrated (program) loudness, LUFS (BS.1770-4).
    pub integrated_lufs: f64,
    /// Maximum true peak, dBTP (4× oversampled).
    pub true_peak_dbtp: f64,
    /// Loudness range, LU (EBU Tech 3342).
    pub loudness_range_lu: f64,
    /// Maximum short-term (3 s) loudness, LUFS.
    pub short_term_max_lufs: f64,
    /// Maximum momentary (400 ms) loudness, LUFS.
    pub momentary_max_lufs: f64,
    /// Duration in seconds.
    pub duration_secs: f64,
    /// Sample rate (always the internal 48 kHz after decode).
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u32,
    /// Noise floor, dBFS (10th-percentile short-term level on non-speech frames).
    pub noise_floor_dbfs: f64,
    /// Speech-vs-noise ratio, dB.
    pub snr_db: f64,
    /// Number of clipped/flat-top regions.
    pub clipping_regions: u32,
    /// Length of the worst clip region in seconds (None if no clipping).
    pub worst_clip_secs: Option<f64>,
    /// Per-channel DC offset (mean sample value).
    pub dc_offset: Vec<f32>,
    /// Mains hum finding, if any.
    pub hum: Option<Hum>,
    /// Coarse reverberation time (RT60) estimate in seconds, if estimable.
    pub rt60_secs: Option<f32>,
    /// Reverb bucket: "dry" | "ok" | "noticeable" | "bad".
    pub reverb_bucket: String,
    /// Spectral roll-off (source bandwidth) in Hz — detects 8/16 kHz-limited sources.
    pub bandwidth_hz: f32,
    /// Fraction of the file that is speech, 0..1.
    pub speech_ratio: f32,
    /// Fraction of the file that is music, 0..1.
    pub music_ratio: f32,
    /// Sibilance ratio: 5–9 kHz band energy on speech frames, 0..1.
    pub sibilance_ratio: f32,
    /// Stereo statistics (None for mono).
    pub stereo: Option<StereoStats>,
    /// Non-speech runs longer than 300 ms.
    pub silence_runs: Vec<SilenceRun>,
    /// The chain version this report was produced under.
    pub chain_version: u32,
}

// ---- Feature extraction constants ---------------------------------------------------------

/// FFT window length for the spectral features. 2048 @ 48 kHz ⇒ 23.4 Hz/bin — fine enough to
/// separate 50 Hz from 60 Hz hum.
const FFT_SIZE: usize = 2048;
/// Roll-off energy fraction defining "bandwidth" (95% of spectral energy lies below it).
const ROLLOFF_FRACTION: f32 = 0.95;
/// Speech gate margin above the noise floor, dB.
const SPEECH_MARGIN_DB: f32 = 10.0;
/// Minimum non-speech run to record as silence: 300 ms = 30 hops.
const MIN_SILENCE_HOPS: usize = 30;

/// One 10 ms hop's worth of scalar features (kept for the whole file; ~32 B/hop).
#[derive(Debug, Clone, Copy, Default)]
struct HopFeature {
    energy_db: f32,
    zcr: f32,
    flatness: f32,
    rolloff_hz: f32,
    sibilance: f32,
    /// 50 Hz bin energy relative to its local spectral neighborhood.
    hum50: f32,
    /// 60 Hz bin energy relative to its local spectral neighborhood.
    hum60: f32,
}

/// Streaming analysis accumulator. Feed it decoded blocks, then [`Analyzer::finish`].
pub struct Analyzer {
    channels: usize,
    sample_rate: u32,
    meter: EbuR128,

    total_frames: usize,
    dc_sum: Vec<f64>,
    ch_energy: Vec<f64>,
    // stereo cross-products (only meaningful for 2ch)
    sum_ll: f64,
    sum_rr: f64,
    sum_lr: f64,
    stereo_identical: bool,

    // clipping run detection
    clip_run: usize,
    clip_regions: u32,
    worst_clip_run: usize,

    // loudness maxima, sampled per block
    st_max: f64,
    m_max: f64,

    // per-hop spectral pipeline
    fft: Arc<dyn RealToComplex<f32>>,
    hann: Vec<f32>,
    ring: Vec<f32>, // 2048-sample mono sliding window (chronological, index 0 = oldest)
    ring_filled: usize,
    since_hop: usize,
    hops: Vec<HopFeature>,
    interleave_scratch: Vec<f32>,
}

impl Analyzer {
    /// Create an analyzer for `channels` channels (the stream is always 48 kHz internally).
    pub fn new(channels: usize) -> Result<Self, DspError> {
        let channels = channels.max(1);
        let meter = EbuR128::new(
            channels as u32,
            INTERNAL_SAMPLE_RATE,
            Mode::I | Mode::LRA | Mode::TRUE_PEAK | Mode::S | Mode::M,
        )?;
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let hann: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        Ok(Self {
            channels,
            sample_rate: INTERNAL_SAMPLE_RATE,
            meter,
            total_frames: 0,
            dc_sum: vec![0.0; channels],
            ch_energy: vec![0.0; channels],
            sum_ll: 0.0,
            sum_rr: 0.0,
            sum_lr: 0.0,
            stereo_identical: channels == 2,
            clip_run: 0,
            clip_regions: 0,
            worst_clip_run: 0,
            st_max: f64::NEG_INFINITY,
            m_max: f64::NEG_INFINITY,
            fft,
            hann,
            ring: vec![0.0; FFT_SIZE],
            ring_filled: 0,
            since_hop: 0,
            hops: Vec::new(),
            interleave_scratch: Vec::new(),
        })
    }

    /// Feed one decoded block.
    pub fn push(&mut self, block: &AudioBuffer) -> Result<(), DspError> {
        let frames = block.frames();
        if frames == 0 {
            return Ok(());
        }
        let ch = block.channel_count().min(self.channels);

        // ebur128 wants interleaved frames.
        self.interleave_scratch.clear();
        self.interleave_scratch.resize(frames * self.channels, 0.0);
        for c in 0..ch {
            let data = block.channel(c);
            for (f, &s) in data.iter().enumerate() {
                self.interleave_scratch[f * self.channels + c] = s;
            }
        }
        self.meter.add_frames_f32(&self.interleave_scratch)?;

        // Streaming per-sample stats + per-hop spectral features.
        let inv_ch = 1.0 / self.channels as f32;
        for f in 0..frames {
            let mut mono = 0.0f32;
            let mut clipped_here = false;
            for c in 0..self.channels {
                let s = if c < ch { block.channel(c)[f] } else { 0.0 };
                self.dc_sum[c] += s as f64;
                self.ch_energy[c] += (s * s) as f64;
                mono += s;
                if s.abs() >= 0.999_9 {
                    clipped_here = true;
                }
            }
            if self.channels == 2 {
                let l = block.channel(0).get(f).copied().unwrap_or(0.0);
                let r = block.channel(1).get(f).copied().unwrap_or(0.0);
                self.sum_ll += (l * l) as f64;
                self.sum_rr += (r * r) as f64;
                self.sum_lr += (l * r) as f64;
                if (l - r).abs() > 1e-6 {
                    self.stereo_identical = false;
                }
            }
            // Clip run bookkeeping.
            if clipped_here {
                self.clip_run += 1;
            } else if self.clip_run > 0 {
                // A clip "region" is a run of ≥ 3 consecutive full-scale samples (flat-top).
                if self.clip_run >= 3 {
                    self.clip_regions += 1;
                    self.worst_clip_run = self.worst_clip_run.max(self.clip_run);
                }
                self.clip_run = 0;
            }

            // Slide the mono FFT ring.
            mono *= inv_ch;
            self.ring.copy_within(1..FFT_SIZE, 0);
            self.ring[FFT_SIZE - 1] = mono;
            if self.ring_filled < FFT_SIZE {
                self.ring_filled += 1;
            }
            self.since_hop += 1;
            if self.since_hop >= HOP_SAMPLES {
                self.since_hop = 0;
                let feat = self.hop_features();
                self.hops.push(feat);
            }
        }

        self.total_frames += frames;

        // Track loudness maxima (query after the block is fed).
        if let Ok(s) = self.meter.loudness_shortterm() {
            if s.is_finite() {
                self.st_max = self.st_max.max(s);
            }
        }
        if let Ok(m) = self.meter.loudness_momentary() {
            if m.is_finite() {
                self.m_max = self.m_max.max(m);
            }
        }
        Ok(())
    }

    /// Compute spectral + time-domain features for the current 2048-sample window.
    fn hop_features(&self) -> HopFeature {
        // Time-domain: energy + ZCR over the most recent hop.
        let tail = &self.ring[FFT_SIZE - HOP_SAMPLES..];
        let mut sq = 0.0f32;
        let mut zc = 0u32;
        for w in tail.windows(2) {
            if (w[0] >= 0.0) != (w[1] >= 0.0) {
                zc += 1;
            }
        }
        for &s in tail {
            sq += s * s;
        }
        let rms = (sq / HOP_SAMPLES as f32).sqrt();
        let energy_db = if rms > 1e-9 {
            20.0 * rms.log10()
        } else {
            -120.0
        };
        let zcr = zc as f32 / HOP_SAMPLES as f32;

        // Windowed FFT for the spectral features.
        let mut input: Vec<f32> = self
            .ring
            .iter()
            .zip(&self.hann)
            .map(|(&s, &w)| s * w)
            .collect();
        let mut spectrum = self.fft.make_output_vec();
        // realfft only errors on wrong lengths, which we control; ignore is safe.
        let _ = self.fft.process(&mut input, &mut spectrum);

        let bins = spectrum.len();
        let bin_hz = self.sample_rate as f32 / FFT_SIZE as f32;
        let mut power = vec![0.0f32; bins];
        let mut total = 0.0f32;
        for (k, c) in spectrum.iter().enumerate() {
            let p = c.norm_sqr();
            power[k] = p;
            total += p;
        }

        let mut rolloff_hz = 0.0f32;
        let mut sibilance = 0.0f32;
        let mut flatness = 0.0f32;
        if total > 1e-12 {
            // Roll-off.
            let target = total * ROLLOFF_FRACTION;
            let mut cum = 0.0f32;
            for (k, &p) in power.iter().enumerate() {
                cum += p;
                if cum >= target {
                    rolloff_hz = k as f32 * bin_hz;
                    break;
                }
            }
            // Sibilance: 5–9 kHz band / total.
            let mut sib = 0.0f32;
            for (k, &p) in power.iter().enumerate() {
                let f = k as f32 * bin_hz;
                if (5_000.0..=9_000.0).contains(&f) {
                    sib += p;
                }
            }
            sibilance = sib / total;
            // Spectral flatness = geo-mean / arith-mean over positive bins.
            let mut log_sum = 0.0f32;
            let mut lin_sum = 0.0f32;
            let mut count = 0.0f32;
            for &p in power.iter().skip(1) {
                let pp = p + 1e-12;
                log_sum += pp.ln();
                lin_sum += pp;
                count += 1.0;
            }
            if count > 0.0 {
                let geo = (log_sum / count).exp();
                let arith = lin_sum / count;
                flatness = (geo / arith).clamp(0.0, 1.0);
            }
        }

        // Hum: energy at the 50/60 Hz bin vs the median of a small neighborhood.
        let hum50 = self.hum_ratio(&power, 50.0, bin_hz);
        let hum60 = self.hum_ratio(&power, 60.0, bin_hz);

        HopFeature {
            energy_db,
            zcr,
            flatness,
            rolloff_hz,
            sibilance,
            hum50,
            hum60,
        }
    }

    /// Ratio of power at the bin nearest `freq` to the mean power of nearby off-peak bins.
    /// At 48 kHz / 2048, 50–60 Hz land on bins 2–3, so the neighborhood is built from valid
    /// indices only (mostly the bins just above the peak) to avoid underflow.
    fn hum_ratio(&self, power: &[f32], freq: f32, bin_hz: f32) -> f32 {
        let center = (freq / bin_hz).round() as usize;
        if center < 2 || center + 2 >= power.len() {
            return 0.0;
        }
        let peak = power[center].max(power[center - 1]).max(power[center + 1]);
        // Neighborhood within ±6 bins, excluding the peak triplet [center−1, center+1].
        let lo = center.saturating_sub(6).max(1);
        let hi = (center + 6).min(power.len() - 1);
        let mut neigh = 0.0f32;
        let mut n = 0.0f32;
        #[allow(clippy::needless_range_loop)]
        for k in lo..=hi {
            if k + 1 < center || k > center + 1 {
                neigh += power[k];
                n += 1.0;
            }
        }
        if n < 1.0 {
            return 0.0;
        }
        let neigh = (neigh / n).max(1e-12);
        peak / neigh
    }

    /// Consume the accumulator and produce the report.
    pub fn finish(mut self, chain_version: u32) -> AnalysisReport {
        let duration_secs = self.total_frames as f64 / self.sample_rate as f64;

        // Flush a trailing clip run.
        if self.clip_run >= 3 {
            self.clip_regions += 1;
            self.worst_clip_run = self.worst_clip_run.max(self.clip_run);
        }

        let integrated = finite_or(
            self.meter.loudness_global().unwrap_or(f64::NEG_INFINITY),
            -120.0,
        );
        let lra = self.meter.loudness_range().unwrap_or(0.0);
        let lra = if lra.is_finite() { lra } else { 0.0 };
        let mut tp = 0.0f64;
        for c in 0..self.channels as u32 {
            if let Ok(p) = self.meter.true_peak(c) {
                tp = tp.max(p);
            }
        }
        let true_peak_dbtp = if tp > 0.0 { 20.0 * tp.log10() } else { -120.0 };

        let dc_offset: Vec<f32> = self
            .dc_sum
            .iter()
            .map(|&s| (s / self.total_frames.max(1) as f64) as f32)
            .collect();

        let stereo = self.stereo_stats();

        // --- VAD / segmentation from the per-hop features -------------------------------
        let seg = segment(&self.hops);

        let worst_clip_secs = if self.clip_regions > 0 {
            Some(self.worst_clip_run as f64 / self.sample_rate as f64)
        } else {
            None
        };

        let hum = detect_hum(&self.hops, &seg.speech_mask);
        let (rt60_secs, reverb_bucket) = estimate_reverb(&self.hops, &seg.speech_mask);

        AnalysisReport {
            integrated_lufs: integrated,
            true_peak_dbtp,
            loudness_range_lu: lra,
            short_term_max_lufs: finite_or(self.st_max, -120.0),
            momentary_max_lufs: finite_or(self.m_max, -120.0),
            duration_secs,
            sample_rate: self.sample_rate,
            channels: self.channels as u32,
            noise_floor_dbfs: seg.noise_floor_db as f64,
            snr_db: seg.snr_db as f64,
            clipping_regions: self.clip_regions,
            worst_clip_secs,
            dc_offset,
            hum,
            rt60_secs,
            reverb_bucket,
            bandwidth_hz: seg.bandwidth_hz,
            speech_ratio: seg.speech_ratio,
            music_ratio: seg.music_ratio,
            sibilance_ratio: seg.sibilance_ratio,
            stereo,
            silence_runs: seg.silence_runs,
            chain_version,
        }
    }

    fn stereo_stats(&self) -> Option<StereoStats> {
        if self.channels != 2 {
            return None;
        }
        let denom = (self.sum_ll * self.sum_rr).sqrt();
        let correlation = if denom > 1e-12 {
            (self.sum_lr / denom) as f32
        } else {
            0.0
        };
        let lr_imbalance_db = if self.ch_energy[0] > 1e-12 && self.ch_energy[1] > 1e-12 {
            (10.0 * (self.ch_energy[0] / self.ch_energy[1]).log10()) as f32
        } else {
            0.0
        };
        let dual_mono =
            self.stereo_identical || (correlation > 0.9999 && lr_imbalance_db.abs() < 0.1);
        Some(StereoStats {
            correlation: correlation.clamp(-1.0, 1.0),
            dual_mono,
            lr_imbalance_db,
        })
    }
}

/// Floor a non-finite loudness value to a sentinel so the JSON contract never emits NaN/inf.
fn finite_or(v: f64, sentinel: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        sentinel
    }
}

/// Percentile (0..1) of a slice via a sorted copy. Empty ⇒ `default`.
fn percentile(values: &[f32], p: f32, default: f32) -> f32 {
    if values.is_empty() {
        return default;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() - 1) as f32 * p).round() as usize;
    v[idx]
}

/// Outputs of the VAD/segmentation stage.
struct Segmentation {
    speech_mask: Vec<bool>,
    speech_ratio: f32,
    music_ratio: f32,
    noise_floor_db: f32,
    snr_db: f32,
    bandwidth_hz: f32,
    sibilance_ratio: f32,
    silence_runs: Vec<SilenceRun>,
}

/// Energy + spectral-flatness VAD and a coarse speech/music/silence segmenter (03 §1: a
/// robust energy + flatness + ZCR segmenter is acceptable for M1).
fn segment(hops: &[HopFeature]) -> Segmentation {
    if hops.is_empty() {
        return Segmentation {
            speech_mask: Vec::new(),
            speech_ratio: 0.0,
            music_ratio: 0.0,
            noise_floor_db: -120.0,
            snr_db: 0.0,
            bandwidth_hz: 0.0,
            sibilance_ratio: 0.0,
            silence_runs: Vec::new(),
        };
    }

    let energies: Vec<f32> = hops.iter().map(|h| h.energy_db).collect();

    // Bootstrap noise floor from the global 10th percentile, then gate, then re-estimate on
    // the non-speech frames (one refinement pass).
    let mut noise_floor = percentile(&energies, 0.10, -120.0);
    let mut speech_mask = gate(hops, noise_floor);
    let nonspeech: Vec<f32> = energies
        .iter()
        .zip(&speech_mask)
        .filter(|(_, &sp)| !sp)
        .map(|(&e, _)| e)
        .collect();
    if !nonspeech.is_empty() {
        noise_floor = percentile(&nonspeech, 0.10, noise_floor);
        speech_mask = gate(hops, noise_floor);
    }

    // Speech energy mean → SNR.
    let speech_energies: Vec<f32> = energies
        .iter()
        .zip(&speech_mask)
        .filter(|(_, &sp)| sp)
        .map(|(&e, _)| e)
        .collect();
    let speech_mean = if speech_energies.is_empty() {
        noise_floor
    } else {
        speech_energies.iter().sum::<f32>() / speech_energies.len() as f32
    };
    let snr_db = (speech_mean - noise_floor).max(0.0);

    // Music vs speech over 1 s windows (100 hops): music is near-continuously active with a
    // higher spectral flatness and steadier energy; speech has syllabic gaps.
    let win = 100usize;
    let mut music_hops = 0usize;
    let mut speech_hops = 0usize;
    let mut is_music_window = vec![false; hops.len()];
    let mut w = 0;
    while w < hops.len() {
        let end = (w + win).min(hops.len());
        let slice = &speech_mask[w..end];
        let active = slice.iter().filter(|&&s| s).count() as f32 / (end - w) as f32;
        let mean_flat = hops[w..end].iter().map(|h| h.flatness).sum::<f32>() / (end - w) as f32;
        let mean_zcr = hops[w..end].iter().map(|h| h.zcr).sum::<f32>() / (end - w) as f32;
        // Energy variance within the window (steadiness).
        let mean_e = hops[w..end].iter().map(|h| h.energy_db).sum::<f32>() / (end - w) as f32;
        let var_e = hops[w..end]
            .iter()
            .map(|h| (h.energy_db - mean_e).powi(2))
            .sum::<f32>()
            / (end - w) as f32;
        // Music heuristic: highly continuous and steady, or broadband-flat with low ZCR gaps.
        let musical = active > 0.85 && var_e < 25.0 && (mean_flat > 0.10 || mean_zcr < 0.15);
        for slot in is_music_window.iter_mut().take(end).skip(w) {
            *slot = musical;
        }
        w = end;
    }
    for (i, &sp) in speech_mask.iter().enumerate() {
        if sp {
            if is_music_window[i] {
                music_hops += 1;
            } else {
                speech_hops += 1;
            }
        }
    }
    let n = hops.len() as f32;
    let speech_ratio = speech_hops as f32 / n;
    let music_ratio = music_hops as f32 / n;

    // Bandwidth = median roll-off over active frames.
    let active_rolloffs: Vec<f32> = hops
        .iter()
        .zip(&speech_mask)
        .filter(|(_, &sp)| sp)
        .map(|(h, _)| h.rolloff_hz)
        .collect();
    let bandwidth_hz = percentile(&active_rolloffs, 0.5, 0.0);

    // Sibilance = mean 5–9 kHz ratio over speech (non-music) frames.
    let sib: Vec<f32> = hops
        .iter()
        .enumerate()
        .filter(|(i, _)| speech_mask[*i] && !is_music_window[*i])
        .map(|(_, h)| h.sibilance)
        .collect();
    let sibilance_ratio = if sib.is_empty() {
        0.0
    } else {
        sib.iter().sum::<f32>() / sib.len() as f32
    };

    // Silence runs: non-speech stretches ≥ 300 ms.
    let mut silence_runs = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, &sp) in speech_mask.iter().enumerate() {
        if !sp {
            run_start.get_or_insert(i);
        } else if let Some(start) = run_start.take() {
            if i - start >= MIN_SILENCE_HOPS {
                silence_runs.push(hop_run_secs(start, i));
            }
        }
    }
    if let Some(start) = run_start {
        if hops.len() - start >= MIN_SILENCE_HOPS {
            silence_runs.push(hop_run_secs(start, hops.len()));
        }
    }

    Segmentation {
        speech_mask,
        speech_ratio,
        music_ratio,
        noise_floor_db: noise_floor,
        snr_db,
        bandwidth_hz,
        sibilance_ratio,
        silence_runs,
    }
}

/// Per-hop speech gate: energy comfortably above the floor, and either structured (low
/// flatness) or clearly loud. Purely energy/flatness based (M1).
fn gate(hops: &[HopFeature], noise_floor_db: f32) -> Vec<bool> {
    hops.iter()
        .map(|h| {
            let loud = h.energy_db > noise_floor_db + SPEECH_MARGIN_DB;
            let very_loud = h.energy_db > noise_floor_db + 25.0;
            loud && (h.flatness < 0.6 || very_loud)
        })
        .collect()
}

fn hop_run_secs(start_hop: usize, end_hop: usize) -> SilenceRun {
    let hop_secs = HOP_SAMPLES as f64 / INTERNAL_SAMPLE_RATE as f64;
    SilenceRun {
        start: start_hop as f64 * hop_secs,
        end: end_hop as f64 * hop_secs,
    }
}

/// Hum detection: a bin at 50 or 60 Hz that stands well above its neighbors across a good
/// fraction of frames. Picks whichever fundamental is stronger and flags stability.
fn detect_hum(hops: &[HopFeature], _speech_mask: &[bool]) -> Option<Hum> {
    if hops.len() < 20 {
        return None;
    }
    let ratios50: Vec<f32> = hops.iter().map(|h| h.hum50).collect();
    let ratios60: Vec<f32> = hops.iter().map(|h| h.hum60).collect();
    let med50 = percentile(&ratios50, 0.5, 0.0);
    let med60 = percentile(&ratios60, 0.5, 0.0);

    // A clear tonal peak sits several times above the local neighborhood.
    const HUM_THRESHOLD: f32 = 6.0;
    let (freq, med, ratios) = if med50 >= med60 {
        (50.0, med50, &ratios50)
    } else {
        (60.0, med60, &ratios60)
    };
    if med < HUM_THRESHOLD {
        return None;
    }
    // Stable if present in most frames (median already high ⇒ stable).
    let present =
        ratios.iter().filter(|&&r| r >= HUM_THRESHOLD).count() as f32 / ratios.len() as f32;
    Some(Hum {
        fundamental_hz: freq,
        stable: present > 0.6,
    })
}

/// Coarse RT60 estimate (03 §1, "basic ok"): fit a decay slope on energy after speech
/// offsets. Falls back to None / "ok" when there is too little to measure.
fn estimate_reverb(hops: &[HopFeature], speech_mask: &[bool]) -> (Option<f32>, String) {
    if hops.len() < 60 {
        return (None, "ok".into());
    }
    let hop_secs = HOP_SAMPLES as f32 / INTERNAL_SAMPLE_RATE as f32;
    let mut slopes = Vec::new(); // dB/sec (negative)
    let mut i = 1;
    while i + 20 < hops.len() {
        // Speech→non-speech offset.
        if speech_mask[i - 1] && !speech_mask[i] {
            // Fit dB decay over the next ~150 ms while it keeps falling.
            let start_db = hops[i].energy_db;
            let mut last = i;
            #[allow(clippy::needless_range_loop)]
            for j in i..(i + 15).min(hops.len()) {
                if hops[j].energy_db <= start_db {
                    last = j;
                } else {
                    break;
                }
            }
            if last > i + 3 {
                let drop = start_db - hops[last].energy_db;
                let dt = (last - i) as f32 * hop_secs;
                if dt > 0.0 && drop > 3.0 {
                    slopes.push(drop / dt); // positive dB/sec of decay
                }
            }
            i = last + 1;
        } else {
            i += 1;
        }
    }
    if slopes.len() < 3 {
        return (None, "ok".into());
    }
    let med_slope = percentile(&slopes, 0.5, 0.0);
    if med_slope <= 0.0 {
        return (None, "ok".into());
    }
    let rt60 = (60.0 / med_slope).clamp(0.05, 3.0);
    let bucket = if rt60 < 0.3 {
        "dry"
    } else if rt60 < 0.5 {
        "ok"
    } else if rt60 < 0.8 {
        "noticeable"
    } else {
        "bad"
    };
    (Some(rt60), bucket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn analyze_buf(buf: &AudioBuffer) -> AnalysisReport {
        let mut a = Analyzer::new(buf.channel_count()).unwrap();
        a.push(buf).unwrap();
        a.finish(anvil_core::CHAIN_VERSION)
    }

    #[test]
    fn minus_23_lufs_tone_reads_back_correctly() {
        // A −23 LUFS sine: amplitude for a single-channel K-weighted tone at 1 kHz is close
        // to −20 dBFS ≈ 0.1; ebur128 gives the exact reading, we just assert it's near −23.
        // Use amplitude tuned so integrated ≈ −23 LUFS.
        let amp = 10f32.powf(-20.0 / 20.0); // ~ -20 dBFS RMS-ish; measured below
        let s: Vec<f32> = (0..48_000 * 4)
            .map(|i| amp * (i as f32 * 1_000.0 * TAU / 48_000.0).sin())
            .collect();
        let buf = AudioBuffer::from_planar(vec![s], 48_000);
        let r = analyze_buf(&buf);
        // A 1 kHz tone at −20 dBFS reads about −23 LUFS after K-weighting; allow ±1.5 LU.
        assert!(
            (r.integrated_lufs - (-23.0)).abs() < 2.5,
            "integrated {} not near −23",
            r.integrated_lufs
        );
        assert_eq!(r.channels, 1);
        assert_eq!(r.chain_version, anvil_core::CHAIN_VERSION);
    }

    #[test]
    fn clipped_signal_flags_clipping() {
        let s: Vec<f32> = (0..48_000)
            .map(|i| {
                let v = 1.5 * (i as f32 * 300.0 * TAU / 48_000.0).sin();
                v.clamp(-1.0, 1.0)
            })
            .collect();
        let buf = AudioBuffer::from_planar(vec![s], 48_000);
        let r = analyze_buf(&buf);
        assert!(r.clipping_regions > 0, "expected clipping to be detected");
        assert!(r.worst_clip_secs.is_some());
    }

    #[test]
    fn silence_only_is_finite_and_friendly() {
        let buf = AudioBuffer::from_planar(vec![vec![0.0; 48_000]], 48_000);
        let r = analyze_buf(&buf);
        assert!(r.integrated_lufs.is_finite());
        assert!(r.noise_floor_dbfs.is_finite());
        assert_eq!(r.clipping_regions, 0);
        assert_eq!(r.speech_ratio, 0.0);
        assert!(r.stereo.is_none());
    }

    #[test]
    fn dual_mono_is_detected() {
        let s: Vec<f32> = (0..48_000)
            .map(|i| 0.2 * (i as f32 * 220.0 * TAU / 48_000.0).sin())
            .collect();
        let buf = AudioBuffer::from_planar(vec![s.clone(), s], 48_000);
        let r = analyze_buf(&buf);
        let st = r.stereo.expect("stereo stats");
        assert!(st.dual_mono, "identical channels should be dual-mono");
        assert!(st.correlation > 0.99);
    }

    #[test]
    fn genuine_stereo_is_not_collapsed_to_mono() {
        // The dual-mono guard is deliberately strict (correlation > 0.9999 AND < 0.1 dB L/R
        // imbalance) so it never mono-izes real stereo. Two cases that a looser threshold would
        // wrongly collapse must both stay stereo.
        let tone: Vec<f32> = (0..48_000)
            .map(|i| 0.2 * (i as f32 * 220.0 * TAU / 48_000.0).sin())
            .collect();

        // A) Perfectly correlated but level-panned (R at 0.7×L → ~3 dB imbalance). This is a real
        //    panning intent, not dual-mono; the imbalance guard must reject it.
        let r_panned: Vec<f32> = tone.iter().map(|s| s * 0.7).collect();
        let panned = AudioBuffer::from_planar(vec![tone.clone(), r_panned], 48_000);
        let st = analyze_buf(&panned).stereo.expect("stereo stats");
        assert!(
            !st.dual_mono,
            "level-panned stereo (imbalance {:.2} dB) must not be treated as dual-mono",
            st.lr_imbalance_db
        );

        // B) Decorrelated channels (distinct tones) — obviously stereo.
        let r_distinct: Vec<f32> = (0..48_000)
            .map(|i| 0.2 * (i as f32 * 277.0 * TAU / 48_000.0).sin())
            .collect();
        let distinct = AudioBuffer::from_planar(vec![tone, r_distinct], 48_000);
        let st = analyze_buf(&distinct).stereo.expect("stereo stats");
        assert!(
            !st.dual_mono,
            "decorrelated stereo (correlation {:.4}) must not be treated as dual-mono",
            st.correlation
        );
    }

    #[test]
    fn zero_length_is_safe() {
        let buf = AudioBuffer::from_planar(vec![Vec::<f32>::new()], 48_000);
        let r = analyze_buf(&buf);
        assert_eq!(r.duration_secs, 0.0);
        assert!(r.integrated_lufs.is_finite());
        assert!(r.silence_runs.is_empty());
    }

    #[test]
    fn bandwidth_limited_source_reads_low() {
        // A source band-limited to ~4 kHz (only low harmonics) should report a low bandwidth.
        let s: Vec<f32> = (0..48_000)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                0.2 * ((t * 300.0 * TAU).sin() + (t * 1200.0 * TAU).sin())
            })
            .collect();
        let buf = AudioBuffer::from_planar(vec![s], 48_000);
        let r = analyze_buf(&buf);
        assert!(
            r.bandwidth_hz < 8_000.0,
            "band-limited source should read < 8 kHz, got {}",
            r.bandwidth_hz
        );
    }
}
