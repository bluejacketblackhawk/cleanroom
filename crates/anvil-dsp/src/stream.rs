//! Streaming, bounded-memory master (M5, 06 §4 RAM budget).
//!
//! The whole-buffer [`crate::master`] decodes the entire file into RAM and materializes the
//! whole output — at a 3-hour stereo master that alone is ~8 GB, far past the < 1.5 GB budget
//! and an outright OOM on the 8 GB floor machine. [`master_to_file`] is the streaming path that
//! never holds more than a bounded window:
//!
//! 1. **Front chain (streaming).** Decode is streamed (`anvil_media::decode_blocks`) and the
//!    front stages (downmix → … → adaptive leveler, [`crate::run_front_stages`]) run on bounded
//!    **overlapping segments** (60 s / 3 s crossfade). The crossfade hides the per-segment state
//!    reset the same way the DFN3 chunker does, and DFN3 itself is now bounded internally. The
//!    post-leveler signal is spilled to a temp file and fed to a streaming integrated-LUFS meter.
//! 2. **Two-pass loudness + limiter (streaming, from the spill).** The temp file — not the
//!    expensive front chain — is re-read to pick the normalize gain and the limiter's
//!    zero-tolerance trim, so DFN3 runs exactly **once**. The look-ahead true-peak limiter runs
//!    as a [`crate::StreamingLimiter`] carrying its window across blocks.
//! 3. **Render (streaming) → sink.** The temp file is streamed a final time, gain-scaled,
//!    limited and handed block-by-block to a [`BlockSink`] (incremental WAV writer or ffmpeg
//!    pipe) so the mastered output is encoded as it is produced and never fully resident.
//!
//! Peak memory is therefore `O(segment + chunk)`, flat in the file duration. The diarized
//! (per-speaker) path stays on the whole-buffer [`crate::master_with_diarization`]: per-speaker
//! leveling is buffer-relative and does not segment cleanly, and it is the shorter-file case.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anvil_core::{BLOCK_SAMPLES, INTERNAL_SAMPLE_RATE};
use anvil_media::{decode_blocks, AudioBuffer};
use anvil_project::{Preset, Tier};
use ebur128::{EbuR128, Mode};

use crate::chain::{
    auto_configure, build_master_report, converge_drive_gain, run_front_stages, ChainConfig,
    GainProbe, LoudnessSnapshot, MasterReport, RenderOutcome,
};
use crate::error::DspError;
use crate::limiter::StreamingLimiter;
use crate::{analyze, VoiceMemory};

/// Front-chain segment length (60 s @ 48 kHz). A whole file is processed in segments this size
/// with [`SEG_OVERLAP`] crossfade, so peak front-chain memory is `O(segment)`.
const SEG_LEN: usize = 60 * INTERNAL_SAMPLE_RATE as usize;
/// Crossfaded overlap between front-chain segments (3 s): longer than the leveler's 3 s AGC
/// window and DFN3's 1 s convergence, so the per-segment state reset lives inside the fade.
const SEG_OVERLAP: usize = 3 * INTERNAL_SAMPLE_RATE as usize;

/// A consumer of mastered blocks as they are produced. The streaming master hands it one bounded
/// [`AudioBuffer`] at a time; the implementor encodes/writes it incrementally (never buffering
/// the whole output). [`finish`](BlockSink::finish) is called once at the end.
pub trait BlockSink {
    /// Consume one block of mastered audio.
    fn write(&mut self, block: &AudioBuffer) -> Result<(), DspError>;
    /// Flush and close the sink.
    fn finish(&mut self) -> Result<(), DspError>;
}

/// The result of a streaming master: the report plus the whole-file scalars the CLI/compliance
/// need — without ever returning the (multi-GB) mastered buffer.
pub struct StreamMasterResult {
    /// The full master report (analysis + before/after + modules + Health Card).
    pub report: MasterReport,
    /// Frames written to the output.
    pub out_frames: u64,
    /// Output channel count (1 if the source was dual-mono and downmixed).
    pub out_channels: usize,
    /// Output sample rate (the engine's internal 48 kHz).
    pub sample_rate: u32,
    /// Whole-file RMS of the mastered output in dBFS (streamed; for the ACX compliance report).
    pub out_rms_dbfs: f64,
}

/// A temp file that deletes itself on drop.
struct TempSpill {
    path: PathBuf,
}

impl TempSpill {
    fn new(tag: &str) -> Result<Self, DspError> {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "anvil-stream-{}-{}-{tag}.f32",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        path.push(unique);
        Ok(Self { path })
    }
}

impl Drop for TempSpill {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn sink_err<E: std::fmt::Display>(e: E) -> DspError {
    DspError::Sink(e.to_string())
}

/// Interleave a planar block into `[l, r, l, r, …]` f32.
fn interleave(block: &AudioBuffer) -> Vec<f32> {
    let ch = block.channel_count();
    let frames = block.frames();
    let mut out = vec![0.0f32; frames * ch];
    for c in 0..ch {
        for (f, &s) in block.channel(c).iter().enumerate() {
            out[f * ch + c] = s;
        }
    }
    out
}

/// Deinterleave `[l, r, l, r, …]` into a planar [`AudioBuffer`].
fn deinterleave(inter: &[f32], channels: usize) -> AudioBuffer {
    let frames = inter.len() / channels.max(1);
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for f in 0..frames {
        for (c, plane) in planar.iter_mut().enumerate() {
            plane.push(inter[f * channels + c]);
        }
    }
    AudioBuffer::from_planar(planar, INTERNAL_SAMPLE_RATE)
}

/// Read a spill file back as interleaved f32 blocks of ~[`BLOCK_SAMPLES`] frames.
struct SpillReader {
    reader: BufReader<File>,
    channels: usize,
}

impl SpillReader {
    fn open(path: &Path, channels: usize) -> Result<Self, DspError> {
        let file = File::open(path).map_err(sink_err)?;
        Ok(Self {
            reader: BufReader::new(file),
            channels,
        })
    }

    /// Next block as interleaved f32, or `None` at EOF.
    fn next_block(&mut self) -> Result<Option<Vec<f32>>, DspError> {
        let want = BLOCK_SAMPLES * self.channels;
        let mut bytes = vec![0u8; want * 4];
        let mut filled = 0usize;
        while filled < bytes.len() {
            match self.reader.read(&mut bytes[filled..]).map_err(sink_err)? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            return Ok(None);
        }
        let n = filled / 4;
        let mut out = vec![0.0f32; n];
        for (i, s) in out.iter_mut().enumerate() {
            *s = f32::from_le_bytes([
                bytes[i * 4],
                bytes[i * 4 + 1],
                bytes[i * 4 + 2],
                bytes[i * 4 + 3],
            ]);
        }
        Ok(Some(out))
    }
}

/// A [`BufWriter`] over a spill file that takes interleaved f32.
struct SpillWriter {
    writer: BufWriter<File>,
}

impl SpillWriter {
    fn create(path: &Path) -> Result<Self, DspError> {
        let file = File::create(path).map_err(sink_err)?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    fn write_interleaved(&mut self, inter: &[f32]) -> Result<(), DspError> {
        let mut bytes = Vec::with_capacity(inter.len() * 4);
        for &s in inter {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.writer.write_all(&bytes).map_err(sink_err)
    }

    fn finish(mut self) -> Result<(), DspError> {
        self.writer.flush().map_err(sink_err)
    }
}

/// Build a [`LoudnessSnapshot`] from a fully-fed meter.
fn snapshot(meter: &EbuR128, channels: usize) -> LoudnessSnapshot {
    let integrated = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let lra = meter.loudness_range().unwrap_or(0.0);
    let mut peak = 0.0f64;
    for c in 0..channels as u32 {
        peak = peak.max(meter.true_peak(c).unwrap_or(0.0));
    }
    LoudnessSnapshot {
        integrated_lufs: if integrated.is_finite() {
            integrated
        } else {
            -120.0
        },
        true_peak_dbtp: if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            -120.0
        },
        loudness_range_lu: if lra.is_finite() { lra } else { 0.0 },
    }
}

/// Pass 1: stream the front chain over overlapping segments, crossfade, spill the post-leveler
/// signal, and measure `before` (raw input) + `l0` (post-leveler integrated LUFS).
struct FrontChainResult {
    out_channels: usize,
    out_frames: u64,
    before: LoudnessSnapshot,
    l0: f64,
}

fn stream_front_chain(
    input: &Path,
    config: &ChainConfig,
    post: &mut SpillWriter,
) -> Result<FrontChainResult, DspError> {
    let mut decoder = decode_blocks(input)?;
    let in_channels = decoder.channel_count().max(1);
    let out_channels = if config.downmix_mono { 1 } else { in_channels };
    let sr = INTERNAL_SAMPLE_RATE;

    let mut before_meter = EbuR128::new(
        in_channels as u32,
        sr,
        Mode::I | Mode::LRA | Mode::TRUE_PEAK,
    )?;
    let mut l0_meter = EbuR128::new(out_channels as u32, sr, Mode::I)?;

    let ovl = SEG_OVERLAP;
    let seg_len = SEG_LEN;
    let hop = seg_len - ovl;

    // Rolling raw-input buffer (bounded: dropped up to the next segment start as we go).
    let mut raw: Vec<Vec<f32>> = vec![Vec::new(); in_channels];
    let mut raw_start = 0usize; // absolute frame index of raw[.][0]
    let mut next_seg = 0usize; // absolute start of the next segment to process
    let mut carry: Option<(usize, Vec<Vec<f32>>)> = None; // (abs start, tail overlap output)
    let mut seg_index = 0usize;
    let mut out_frames: u64 = 0;
    let mut eof = false;

    loop {
        // Fill until we can form a full segment [next_seg, next_seg+seg_len) or hit EOF.
        while !eof && (raw_start + raw[0].len()) < next_seg + seg_len {
            match decoder.next() {
                Some(block) => {
                    let block = block?;
                    before_meter.add_frames_f32(&interleave(&block))?;
                    for (c, plane) in raw.iter_mut().enumerate() {
                        if let Some(src) = block.planar().get(c) {
                            plane.extend_from_slice(src);
                        }
                    }
                }
                None => eof = true,
            }
        }

        let avail_end = raw_start + raw[0].len();
        let seg_end = (next_seg + seg_len).min(avail_end);
        if seg_end <= next_seg {
            break; // nothing left
        }
        let is_first = seg_index == 0;
        let is_last = eof && avail_end <= next_seg + seg_len;

        // Extract [next_seg, seg_end) and run the front stages on it.
        let off = next_seg - raw_start;
        let seg_len_actual = seg_end - next_seg;
        let seg_in = AudioBuffer::from_planar(
            raw.iter()
                .map(|c| c[off..off + seg_len_actual].to_vec())
                .collect(),
            sr,
        );
        let (seg_out, _, _) = run_front_stages(&seg_in, config, sr);
        let so = seg_out.planar();

        // Finalize [next_seg, emit_end); crossfade the head with the previous segment's tail.
        let emit_end = if is_last { seg_end } else { seg_end - ovl };
        let mut outp: Vec<Vec<f32>> = (0..out_channels)
            .map(|_| Vec::with_capacity(emit_end - next_seg))
            .collect();
        for (c, plane) in outp.iter_mut().enumerate() {
            for abs in next_seg..emit_end {
                let i = abs - next_seg;
                let val = if !is_first && abs < next_seg + ovl {
                    let (carry_start, carry_out) = carry.as_ref().expect("carry after first seg");
                    let ci = abs - carry_start;
                    let w_in = (i as f32 + 0.5) / ovl as f32;
                    carry_out[c][ci] * (1.0 - w_in) + so[c][i] * w_in
                } else {
                    so[c][i]
                };
                plane.push(val);
            }
        }

        // Hold this segment's tail overlap for the next segment's crossfade.
        carry = if is_last {
            None
        } else {
            let tail_lo = emit_end - next_seg;
            let tail_hi = seg_end - next_seg;
            Some((
                emit_end,
                so.iter().map(|c| c[tail_lo..tail_hi].to_vec()).collect(),
            ))
        };

        // Emit: spill + integrated meter.
        let block = AudioBuffer::from_planar(outp, sr);
        let inter = interleave(&block);
        post.write_interleaved(&inter)?;
        l0_meter.add_frames_f32(&inter)?;
        out_frames += (emit_end - next_seg) as u64;

        next_seg += hop;
        seg_index += 1;
        // Drop raw we will never revisit (< next_seg).
        let drop_to = next_seg.min(avail_end);
        if drop_to > raw_start {
            let d = drop_to - raw_start;
            for plane in raw.iter_mut() {
                plane.drain(0..d.min(plane.len()));
            }
            raw_start = drop_to;
        }
        if is_last {
            break;
        }
    }

    Ok(FrontChainResult {
        out_channels,
        out_frames,
        before: snapshot(&before_meter, in_channels),
        l0: l0_meter.loudness_global().unwrap_or(f64::NEG_INFINITY),
    })
}

/// Stream the spill → ×`gain_db` → look-ahead limiter (no trim) → meters. Returns the integrated
/// loudness and the zero-tolerance trim (`ceiling / measured_true_peak`) the render pass applies.
fn measure_loudness_pass(
    spill: &Path,
    channels: usize,
    config: &ChainConfig,
    gain_db: f64,
) -> Result<(f64, f32), DspError> {
    let g = 10f32.powf(gain_db as f32 / 20.0);
    let mut reader = SpillReader::open(spill, channels)?;
    let mut lim = StreamingLimiter::new(config.limiter, INTERNAL_SAMPLE_RATE, channels);
    let mut i_meter = EbuR128::new(channels as u32, INTERNAL_SAMPLE_RATE, Mode::I)?;
    let mut tp_meter = EbuR128::new(channels as u32, INTERNAL_SAMPLE_RATE, Mode::TRUE_PEAK)?;

    while let Some(mut inter) = reader.next_block()? {
        for s in inter.iter_mut() {
            *s *= g;
        }
        let block = deinterleave(&inter, channels);
        let limited = lim.push(&block);
        if limited.frames() > 0 {
            let li = interleave(&limited);
            i_meter.add_frames_f32(&li)?;
            tp_meter.add_frames_f32(&li)?;
        }
    }
    let tail = lim.flush();
    if tail.frames() > 0 {
        let ti = interleave(&tail);
        i_meter.add_frames_f32(&ti)?;
        tp_meter.add_frames_f32(&ti)?;
    }

    let ceiling = 10f32.powf(config.limiter.ceiling_dbtp / 20.0);
    let mut peak = 0.0f64;
    for c in 0..channels as u32 {
        peak = peak.max(tp_meter.true_peak(c).unwrap_or(0.0));
    }
    let trim = if peak > ceiling as f64 && peak.is_finite() {
        (ceiling as f64 / peak) as f32
    } else {
        1.0
    };
    let l_pre = i_meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let l_final = if l_pre.is_finite() {
        l_pre + 20.0 * (trim as f64).log10()
    } else {
        l_pre
    };
    Ok((l_final, trim))
}

/// Feed one mastered block to the `after` meter + RMS accumulator, then to the sink.
fn emit_block(
    block: &AudioBuffer,
    after: &mut EbuR128,
    sum_sq: &mut f64,
    count: &mut u64,
    sink: &mut dyn BlockSink,
) -> Result<(), DspError> {
    let inter = interleave(block);
    after.add_frames_f32(&inter)?;
    for &s in &inter {
        *sum_sq += f64::from(s) * f64::from(s);
        *count += 1;
    }
    sink.write(block)
}

/// Final pass: stream the spill → ×`gain_db` → limiter (with `trim`) → sink; measure `after`.
fn render_pass(
    spill: &Path,
    channels: usize,
    config: &ChainConfig,
    gain_db: f64,
    trim: f32,
    sink: &mut dyn BlockSink,
) -> Result<(LoudnessSnapshot, f64), DspError> {
    let g = 10f32.powf(gain_db as f32 / 20.0);
    let mut reader = SpillReader::open(spill, channels)?;
    let mut lim = StreamingLimiter::new(config.limiter, INTERNAL_SAMPLE_RATE, channels);
    lim.set_trim(trim);
    let mut after = EbuR128::new(
        channels as u32,
        INTERNAL_SAMPLE_RATE,
        Mode::I | Mode::LRA | Mode::TRUE_PEAK,
    )?;
    let mut sum_sq = 0.0f64;
    let mut count: u64 = 0;

    while let Some(mut inter) = reader.next_block()? {
        for s in inter.iter_mut() {
            *s *= g;
        }
        let block = deinterleave(&inter, channels);
        let limited = lim.push(&block);
        if limited.frames() > 0 {
            emit_block(&limited, &mut after, &mut sum_sq, &mut count, sink)?;
        }
    }
    let tail = lim.flush();
    if tail.frames() > 0 {
        emit_block(&tail, &mut after, &mut sum_sq, &mut count, sink)?;
    }
    sink.finish()?;

    let snap = snapshot(&after, channels);
    let rms_dbfs = if count > 0 {
        let mean = sum_sq / count as f64;
        if mean > 0.0 {
            10.0 * mean.log10()
        } else {
            -120.0
        }
    } else {
        -120.0
    };
    Ok((snap, rms_dbfs))
}

/// Master `input` end-to-end **streaming**, encoding the result into `sink` block-by-block, and
/// return the report plus the whole-file scalars — without ever holding the whole decoded input
/// or the whole mastered output in RAM (the M5 < 1.5 GB / 3-h budget). Single-speaker path only;
/// diarized masters use [`crate::master_with_diarization`].
pub fn master_to_file(
    input: &Path,
    preset: &Preset,
    tier: Tier,
    sink: &mut dyn BlockSink,
) -> Result<StreamMasterResult, DspError> {
    // Streaming analysis pass (already O(1) in duration) → the auto-decision.
    let report = analyze(input)?;
    if report.duration_secs <= 0.0 {
        return Err(DspError::Empty);
    }
    let config = auto_configure(&report, preset, tier);

    // Pass 1: front chain → spill + before/l0.
    let post = TempSpill::new("post")?;
    let front = {
        let mut writer = SpillWriter::create(&post.path)?;
        let front = stream_front_chain(input, &config, &mut writer)?;
        writer.finish()?;
        front
    };
    if front.out_frames == 0 {
        return Err(DspError::Empty);
    }
    let channels = front.out_channels;

    // Loudness normalize (03 §4.9), streamed from the spill so DFN3 runs only once: drive make-up
    // gain into the limiter and converge the *limited* loudness onto target (shared with the
    // whole-buffer path via `chain::converge_drive_gain`). Each probe is a cheap spill re-read —
    // the front chain / DFN3 never re-runs — so the bounded iteration stays within the RAM/perf
    // budget. The probe returns the identical `l_final` the whole-buffer probe would, keeping the
    // two paths in loudness parity.
    let target = config.target_lufs as f64;
    let solution = converge_drive_gain(target, front.l0, |gain_db| {
        let (l_final, trim) = measure_loudness_pass(&post.path, channels, &config, gain_db)?;
        Ok(GainProbe { l_final, trim })
    })?;
    if !solution.converged {
        tracing::warn!(
            target_lufs = target,
            reached_lufs = solution.l_final,
            "streaming loudness could not converge onto target within the drive cap; \
             shipping closest ({:.2} LU off)",
            target - solution.l_final
        );
    }

    // Final render pass → sink.
    let (after, out_rms_dbfs) = render_pass(
        &post.path,
        channels,
        &config,
        solution.gain_db,
        solution.trim,
        sink,
    )?;

    // Assemble the report (build_master_report reads only the loudness snapshots + gains).
    let outcome = RenderOutcome {
        audio: AudioBuffer::new(channels, INTERNAL_SAMPLE_RATE),
        before: front.before,
        after,
        cache_hits: Vec::new(),
        speaker_gains: Vec::new(),
        voice_memory: VoiceMemory::default(),
    };
    let master_report = build_master_report(report, &config, &outcome);

    Ok(StreamMasterResult {
        report: master_report,
        out_frames: front.out_frames,
        out_channels: channels,
        sample_rate: INTERNAL_SAMPLE_RATE,
        out_rms_dbfs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that just counts what it is handed (proves the streaming path drives it).
    #[derive(Default)]
    struct CountingSink {
        blocks: usize,
        frames: u64,
        channels: usize,
        finished: bool,
    }
    impl BlockSink for CountingSink {
        fn write(&mut self, block: &AudioBuffer) -> Result<(), DspError> {
            self.blocks += 1;
            self.frames += block.frames() as u64;
            self.channels = block.channel_count();
            Ok(())
        }
        fn finish(&mut self) -> Result<(), DspError> {
            self.finished = true;
            Ok(())
        }
    }

    /// A sink that collects every block into one buffer (test-only; the whole point of the
    /// streaming path is *not* to do this in production).
    #[derive(Default)]
    struct CollectSink {
        planar: Vec<Vec<f32>>,
        finished: bool,
    }
    impl BlockSink for CollectSink {
        fn write(&mut self, block: &AudioBuffer) -> Result<(), DspError> {
            if self.planar.is_empty() {
                self.planar = vec![Vec::new(); block.channel_count()];
            }
            for (c, plane) in self.planar.iter_mut().enumerate() {
                plane.extend_from_slice(block.channel(c));
            }
            Ok(())
        }
        fn finish(&mut self) -> Result<(), DspError> {
            self.finished = true;
            Ok(())
        }
    }

    fn write_wav(path: &Path, planar: &[Vec<f32>]) {
        let spec = hound::WavSpec {
            channels: planar.len() as u16,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let frames = planar[0].len();
        for f in 0..frames {
            for plane in planar {
                w.write_sample(plane[f]).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn noisy_speech(secs: usize) -> Vec<Vec<f32>> {
        use std::f32::consts::TAU;
        let n = 48_000 * secs;
        let mut seed = 0x1357_9BDFu32;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let t = i as f32 / 48_000.0;
                let env = 0.5 + 0.5 * (t * 4.0 * TAU).sin();
                let voice = (t * 140.0 * TAU).sin() + 0.4 * (t * 280.0 * TAU).sin();
                0.2 * env * voice + 0.05 * noise
            })
            .collect();
        vec![s.clone(), s]
    }

    #[test]
    fn streaming_master_hits_target_and_ceiling() {
        let tmp = std::env::temp_dir().join(format!("anvil-strm-test-{}.wav", std::process::id()));
        write_wav(&tmp, &noisy_speech(6));
        let preset = Preset::default(); // −16 LUFS, −1 dBTP
        let mut sink = CountingSink::default();
        let res = master_to_file(&tmp, &preset, Tier::Standard, &mut sink).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert!(sink.finished, "sink must be finished");
        assert_eq!(sink.frames, res.out_frames, "sink saw every frame");
        assert_eq!(sink.channels, res.out_channels);
        assert!(
            (res.report.after.integrated_lufs - preset.target_lufs as f64).abs() <= 0.5,
            "streaming integrated {} off target {}",
            res.report.after.integrated_lufs,
            preset.target_lufs
        );
        assert!(
            res.report.after.true_peak_dbtp <= preset.true_peak_ceiling_dbtp as f64 + 0.01,
            "streaming true peak {} over ceiling",
            res.report.after.true_peak_dbtp
        );
    }

    /// A fixed-config streaming master is bit-identical on a re-render (determinism gate, the
    /// streaming analogue of `full_master_is_deterministic_double_render`).
    #[test]
    fn streaming_master_is_deterministic() {
        let tmp = std::env::temp_dir().join(format!("anvil-strm-det-{}.wav", std::process::id()));
        write_wav(&tmp, &noisy_speech(5));
        let preset = Preset::default();
        let mut a = CollectSink::default();
        let mut b = CollectSink::default();
        master_to_file(&tmp, &preset, Tier::Standard, &mut a).unwrap();
        master_to_file(&tmp, &preset, Tier::Standard, &mut b).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(
            a.planar, b.planar,
            "streaming double render must be bit-identical"
        );
    }

    /// On a short file (one front-chain segment) the streaming master matches the whole-buffer
    /// [`crate::master`] closely: the front chain is the *same* code path, the streaming limiter
    /// is bit-identical to the whole-buffer one, and the two-pass loudness math is the same. The
    /// residual is only meter/measurement float order — well under a milli-unit.
    #[test]
    fn streaming_master_matches_whole_buffer_on_short_file() {
        let tmp = std::env::temp_dir().join(format!("anvil-strm-cmp-{}.wav", std::process::id()));
        write_wav(&tmp, &noisy_speech(8));
        let preset = Preset::default();

        let mut sink = CollectSink::default();
        let streamed = master_to_file(&tmp, &preset, Tier::Standard, &mut sink).unwrap();
        let whole = crate::master(&tmp, &preset, Tier::Standard).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(sink.planar.len(), whole.audio.channel_count());
        // Loudness/ceiling land on the same numbers.
        assert!(
            (streamed.report.after.integrated_lufs - whole.report.after.integrated_lufs).abs()
                < 0.2,
            "integrated {} vs {}",
            streamed.report.after.integrated_lufs,
            whole.report.after.integrated_lufs
        );
        // And the samples track the whole-buffer master tightly.
        let n = sink.planar[0].len().min(whole.audio.frames());
        let mut maxerr = 0.0f32;
        for c in 0..sink.planar.len() {
            for i in 0..n {
                maxerr = maxerr.max((sink.planar[c][i] - whole.audio.channel(c)[i]).abs());
            }
        }
        assert!(
            maxerr < 5e-3,
            "streaming vs whole-buffer master max sample error {maxerr}"
        );
    }

    /// A peaky, high-crest source (in-phase harmonics ≈ a pulse train), which at target loudness
    /// pushes the true peak over the ceiling and forces the limiter to do the work. The old
    /// single flat-trim correction left this kind of content several LU under target.
    fn high_crest_pulse(secs: usize) -> Vec<Vec<f32>> {
        use std::f32::consts::TAU;
        let n = 48_000 * secs;
        let f0 = 120.0;
        // Fewer harmonics than the whole-buffer unit test: through the *full* auto-configured
        // front chain (denoise/AutoEQ reshape the spectrum) this stays reachable within the drive
        // cap while still crest-y enough to push the true peak onto the ceiling.
        let harmonics = 6;
        let mut seed = 0x51ED_2A17u32;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let mut v = 0.0f32;
                for k in 1..=harmonics {
                    v += (t * f0 * k as f32 * TAU).cos();
                }
                v /= harmonics as f32;
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                0.8 * v + 0.02 * noise
            })
            .collect();
        vec![s.clone(), s]
    }

    /// High-crest content converges onto target on **both** master paths, and the two paths agree
    /// on the resulting loudness. This is the end-to-end guard for the flat-trim undershoot fix
    /// (both paths share `chain::converge_drive_gain`) and the streaming↔whole-buffer loudness
    /// parity assertion.
    #[test]
    fn streaming_and_whole_buffer_converge_and_agree_on_high_crest() {
        let tmp = std::env::temp_dir().join(format!("anvil-strm-crest-{}.wav", std::process::id()));
        write_wav(&tmp, &high_crest_pulse(6));
        let preset = Preset::default(); // −16 LUFS, −1 dBTP
        let target = preset.target_lufs as f64;
        let ceiling = preset.true_peak_ceiling_dbtp as f64;

        let mut sink = CollectSink::default();
        let streamed = master_to_file(&tmp, &preset, Tier::Standard, &mut sink).unwrap();
        let whole = crate::master(&tmp, &preset, Tier::Standard).unwrap();
        let _ = std::fs::remove_file(&tmp);

        for (label, after) in [
            ("streaming", streamed.report.after),
            ("whole-buffer", whole.report.after),
        ] {
            // Converged onto target (inside the ±0.5 contract) ...
            assert!(
                (after.integrated_lufs - target).abs() <= 0.5,
                "{label} integrated {} not within 0.5 LU of target {}",
                after.integrated_lufs,
                target
            );
            // ... with the true peak never over the ceiling.
            assert!(
                after.true_peak_dbtp <= ceiling + 0.01,
                "{label} true peak {} over ceiling {}",
                after.true_peak_dbtp,
                ceiling
            );
        }
        // The two paths land on the *same* loudness (they share the convergence and the limiter is
        // bit-exact across them); float-order in the meters is all that separates them.
        assert!(
            (streamed.report.after.integrated_lufs - whole.report.after.integrated_lufs).abs()
                < 0.1,
            "streaming↔whole-buffer loudness parity: {} vs {}",
            streamed.report.after.integrated_lufs,
            whole.report.after.integrated_lufs
        );
    }
}
