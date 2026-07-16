//! Decoding: symphonia first, ffmpeg sidecar as fallback, everything normalized to the
//! engine's internal format (planar f32 @ [`anvil_core::INTERNAL_SAMPLE_RATE`]).
//!
//! Two entry points:
//! - [`decode_to_buffer`] — whole file in one [`AudioBuffer`], for small files and tests.
//! - [`decode_blocks`] — a [`BlockDecoder`] iterator yielding ~[`anvil_core::BLOCK_SAMPLES`]
//!   frames at a time so a multi-hour file never fully resides in RAM. Only a few blocks'
//!   worth of samples are buffered at any moment (well under the M0 500 MB budget).
//!
//! Sample-rate conversion to 48 kHz uses rubato's `FftFixedIn`. When the source is already
//! 48 kHz the resampler is bypassed entirely, so those files decode bit-for-bit.

use std::path::Path;

use anvil_core::{BLOCK_SAMPLES, INTERNAL_SAMPLE_RATE};
use rubato::{FftFixedIn, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::MediaError;
use crate::probe::pick_audio_track;
use crate::sidecar::FfmpegSidecar;
use crate::AudioBuffer;

/// rubato input chunk size (source-rate frames per `process` call). Small enough that the
/// pending-sample buffers stay tiny; `sub_chunks = 2` keeps the FFT sizes reasonable.
const RESAMPLER_CHUNK_IN: usize = 1024;
const RESAMPLER_SUB_CHUNKS: usize = 2;

/// Decode an entire file into a single [`AudioBuffer`] at 48 kHz.
///
/// Convenience wrapper over [`decode_blocks`]; prefer the streaming API for large inputs.
pub fn decode_to_buffer(path: &Path) -> Result<AudioBuffer, MediaError> {
    let mut decoder = decode_blocks(path)?;
    let channels = decoder.channel_count();
    let mut planar: Vec<Vec<f32>> = vec![Vec::new(); channels.max(1)];

    for block in &mut decoder {
        let block = block?;
        for (dst, src) in planar.iter_mut().zip(block.planar()) {
            dst.extend_from_slice(src);
        }
    }
    Ok(AudioBuffer::from_planar(planar, INTERNAL_SAMPLE_RATE))
}

/// Open a streaming block decoder for `path`, resolving symphonia-vs-ffmpeg up front.
pub fn decode_blocks(path: &Path) -> Result<BlockDecoder, MediaError> {
    match SymphoniaBlocks::open(path) {
        Ok(blocks) => Ok(BlockDecoder {
            inner: BlockInner::Symphonia(Box::new(blocks)),
        }),
        // symphonia couldn't handle it: an unsupported container/codec (mkv/webm/mov, exotic
        // codecs), no audio track, or a probe that ran off the end of a stream it didn't
        // recognize (symphonia surfaces that last case as an I/O "end of stream", not an
        // Unsupported error). Any of these means "not symphonia's job" — hand off to the
        // ffmpeg sidecar. Guard on the file actually existing so a genuinely missing or
        // unreadable path returns symphonia's real error, not a confusing sidecar failure.
        Err(sym_err) => {
            if !path.is_file() {
                return Err(sym_err);
            }
            let sidecar = FfmpegSidecar::locate()?;
            Ok(BlockDecoder {
                inner: BlockInner::Ffmpeg(sidecar.decode_blocks(path)?),
            })
        }
    }
}

/// Streaming decoder yielding planar-f32 @ 48 kHz blocks of ~[`BLOCK_SAMPLES`] frames.
///
/// Backed by either symphonia or the ffmpeg sidecar; the caller does not need to know which.
pub struct BlockDecoder {
    inner: BlockInner,
}

enum BlockInner {
    Symphonia(Box<SymphoniaBlocks>),
    Ffmpeg(crate::sidecar::FfmpegBlocks),
}

impl BlockDecoder {
    /// Channel count of the decoded stream (constant across blocks).
    pub fn channel_count(&self) -> usize {
        match &self.inner {
            BlockInner::Symphonia(s) => s.channels,
            BlockInner::Ffmpeg(f) => f.channels(),
        }
    }
}

impl Iterator for BlockDecoder {
    type Item = Result<AudioBuffer, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            BlockInner::Symphonia(s) => s.next(),
            BlockInner::Ffmpeg(f) => f.next(),
        }
    }
}

/// The demux+decode half: pulls one decoded packet at a time from symphonia and hands back
/// source-rate planar f32. Holds only the current packet.
struct RawSymphonia {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
}

impl RawSymphonia {
    /// Open `path` and return the reader plus the source rate and (optional) channel count
    /// read from codec parameters.
    fn open(path: &Path) -> Result<(Self, u32, Option<usize>), MediaError> {
        let file = std::fs::File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        let format = probed.format;

        let track = pick_audio_track(format.tracks())
            .ok_or_else(|| MediaError::NoAudioTrack(path.display().to_string()))?;
        let track_id = track.id;
        let source_rate = track.codec_params.sample_rate.ok_or_else(|| {
            MediaError::UnsupportedFormat("audio track has no sample rate".into())
        })?;
        let channels = track.codec_params.channels.map(|c| c.count());

        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;

        Ok((
            Self {
                format,
                decoder,
                track_id,
            },
            source_rate,
            channels,
        ))
    }

    /// Decode the next audio packet into source-rate planar f32, or `None` at end of stream.
    /// Bad packets are skipped (symphonia's recommendation); EOF surfaces as `None`.
    fn next_raw(&mut self) -> Result<Option<Vec<Vec<f32>>>, MediaError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                // A reset request means the track layout changed; for our single-track
                // decode we treat it as end-of-stream rather than reconfiguring mid-file.
                Err(SymError::ResetRequired) => return Ok(None),
                Err(e) => return Err(e.into()),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let channels = spec.channels.count();
                    if channels == 0 {
                        continue;
                    }
                    let mut sample_buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                    sample_buf.copy_interleaved_ref(decoded);

                    let interleaved = sample_buf.samples();
                    if interleaved.is_empty() {
                        continue;
                    }
                    let frames = interleaved.len() / channels;
                    let mut planar = vec![Vec::with_capacity(frames); channels];
                    for (i, &sample) in interleaved.iter().enumerate() {
                        planar[i % channels].push(sample);
                    }
                    return Ok(Some(planar));
                }
                Err(SymError::DecodeError(_)) => continue,
                Err(SymError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// A rubato `FftFixedIn` plus the bookkeeping to drive it as a fixed-chunk stream.
struct Resampling {
    inner: FftFixedIn<f32>,
    /// Exact input frames consumed per `process` call.
    chunk_in: usize,
    /// Leading output frames still to be discarded to compensate for the resampler's
    /// startup delay (so 48 kHz output stays time-aligned with the source).
    skip_out: usize,
}

/// The full symphonia streaming pipeline: decode -> (optional) resample -> 48 kHz blocks.
struct SymphoniaBlocks {
    raw: RawSymphonia,
    resampler: Option<Resampling>,
    channels: usize,
    /// Source native rate, used to compute the exact resampled length at EOF.
    source_rate: u32,
    /// Pending source-rate samples not yet fed to the resampler (empty when 48 kHz).
    in_buf: Vec<Vec<f32>>,
    /// Ready 48 kHz output samples awaiting emission as blocks.
    out_buf: Vec<Vec<f32>>,
    /// Total source frames fed through the resampler (for exact output-length targeting).
    source_frames: usize,
    /// Total 48 kHz frames already emitted to the caller.
    emitted: usize,
    /// Exact 48 kHz output length, fixed once the source ends. `None` while more may arrive
    /// and on the 48 kHz identity path (which needs no length correction).
    resample_target: Option<usize>,
    source_done: bool,
    flushed: bool,
}

impl SymphoniaBlocks {
    fn open(path: &Path) -> Result<Self, MediaError> {
        let (mut raw, source_rate, channel_hint) = RawSymphonia::open(path)?;

        // Prime one block so the exact channel count comes from real decoded data (some
        // containers omit it from codec params) and the resampler can be sized correctly.
        let primed = raw.next_raw()?;
        let channels = primed
            .as_ref()
            .map(|b| b.len())
            .or(channel_hint)
            .unwrap_or(1)
            .max(1);

        let resampler = if source_rate != INTERNAL_SAMPLE_RATE {
            let inner = FftFixedIn::<f32>::new(
                source_rate as usize,
                INTERNAL_SAMPLE_RATE as usize,
                RESAMPLER_CHUNK_IN,
                RESAMPLER_SUB_CHUNKS,
                channels,
            )?;
            let chunk_in = inner.input_frames_next();
            let skip_out = inner.output_delay();
            Some(Resampling {
                inner,
                chunk_in,
                skip_out,
            })
        } else {
            None
        };

        let mut blocks = Self {
            raw,
            resampler,
            channels,
            source_rate,
            in_buf: vec![Vec::new(); channels],
            out_buf: vec![Vec::new(); channels],
            source_frames: 0,
            emitted: 0,
            resample_target: None,
            source_done: false,
            flushed: false,
        };

        if let Some(block) = primed {
            blocks.feed_source(block);
        }
        Ok(blocks)
    }

    /// Route a freshly decoded source block into the pipeline: straight to output when no
    /// resampling is needed, otherwise into the resampler's input buffer (counting frames so
    /// the exact resampled length is known at EOF).
    fn feed_source(&mut self, block: Vec<Vec<f32>>) {
        let frames = block.first().map_or(0, Vec::len);
        let dst = if self.resampler.is_some() {
            self.source_frames += frames;
            &mut self.in_buf
        } else {
            &mut self.out_buf
        };
        for (channel, src) in dst.iter_mut().zip(block) {
            channel.extend(src);
        }
    }

    /// Fix the exact 48 kHz output length from the total source frames seen. Called once, the
    /// moment the source stream ends, so the resampler's zero-padded final chunk and delayed
    /// tail don't inflate the reported duration.
    fn set_resample_target(&mut self) {
        if self.resampler.is_some() && self.resample_target.is_none() {
            let target = (self.source_frames as u128 * INTERNAL_SAMPLE_RATE as u128
                / self.source_rate as u128) as usize;
            self.resample_target = Some(target);
        }
    }

    fn out_ready(&self) -> usize {
        self.out_buf.first().map_or(0, Vec::len)
    }

    fn done(&self) -> bool {
        match self.resampler {
            None => self.source_done,
            Some(_) => self.source_done && self.flushed,
        }
    }

    /// Append resampler output to the ready buffer, first discarding any remaining
    /// startup-delay frames.
    fn push_output(&mut self, mut produced: Vec<Vec<f32>>) {
        if let Some(r) = self.resampler.as_mut() {
            if r.skip_out > 0 {
                let available = produced.first().map_or(0, Vec::len);
                let drop = r.skip_out.min(available);
                if drop > 0 {
                    for channel in &mut produced {
                        channel.drain(0..drop);
                    }
                    r.skip_out -= drop;
                }
            }
        }
        for (dst, src) in self.out_buf.iter_mut().zip(produced) {
            dst.extend(src);
        }
    }

    /// Make one unit of forward progress toward filling the output buffer.
    fn pump(&mut self) -> Result<(), MediaError> {
        if self.resampler.is_none() {
            match self.raw.next_raw()? {
                Some(block) => self.feed_source(block),
                None => self.source_done = true,
            }
            return Ok(());
        }

        let chunk_in = self.resampler.as_ref().unwrap().chunk_in;
        let pending = self.in_buf.first().map_or(0, Vec::len);

        if pending >= chunk_in {
            let chunk: Vec<Vec<f32>> = self
                .in_buf
                .iter_mut()
                .map(|c| c.drain(0..chunk_in).collect())
                .collect();
            let produced = self
                .resampler
                .as_mut()
                .unwrap()
                .inner
                .process(&chunk, None)?;
            self.push_output(produced);
        } else if !self.source_done {
            match self.raw.next_raw()? {
                Some(block) => self.feed_source(block),
                None => {
                    self.source_done = true;
                    self.set_resample_target();
                }
            }
        } else if !self.flushed {
            // Source exhausted: flush the short final chunk (zero-padded internally), then
            // push out the resampler's delayed tail.
            if pending > 0 {
                let leftover: Vec<Vec<f32>> = self.in_buf.iter_mut().map(std::mem::take).collect();
                let produced = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .inner
                    .process_partial(Some(&leftover), None)?;
                self.push_output(produced);
            } else {
                let produced = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .inner
                    .process_partial::<Vec<f32>>(None, None)?;
                self.push_output(produced);
                self.flushed = true;
            }
        }
        Ok(())
    }

    fn fill(&mut self) -> Result<(), MediaError> {
        while self.out_ready() < BLOCK_SAMPLES && !self.done() {
            self.pump()?;
        }
        Ok(())
    }
}

impl Iterator for SymphoniaBlocks {
    type Item = Result<AudioBuffer, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.fill() {
            return Some(Err(e));
        }
        let mut available = self.out_ready();
        // On the resampling path, never emit past the exact target length — that trims the
        // zero-padded tail the resampler adds while flushing.
        if let Some(target) = self.resample_target {
            available = available.min(target.saturating_sub(self.emitted));
        }
        if available == 0 {
            return None;
        }
        let take = available.min(BLOCK_SAMPLES);
        let planar: Vec<Vec<f32>> = self
            .out_buf
            .iter_mut()
            .map(|c| c.drain(0..take).collect())
            .collect();
        self.emitted += take;
        Some(Ok(AudioBuffer::from_planar(planar, INTERNAL_SAMPLE_RATE)))
    }
}
