//! cpal output engine and transport (ADR-010).
//!
//! ## Threading model
//!
//! cpal drives its data callback from its own realtime backend thread, and a `cpal::Stream`
//! is not `Send` on every host — so the engine cannot simply hold one behind a Tauri
//! `State`. Instead:
//!
//! - A dedicated **audio thread** owns the output device and the current `Stream`. It
//!   blocks on a command channel between loads; the `Stream` it holds keeps calling its
//!   own callback the whole time.
//! - Transport state ([`Transport`]) is a small `Arc` of atomics shared with the callback.
//!   `play`/`pause`/`stop`/`seek` are plain atomic writes from any thread — no locking, no
//!   round-trip to the audio thread, so they are effectively instant.
//! - Loading a file resamples/interleaves it to the device format on the caller's thread,
//!   then hands the immutable buffer to the audio thread, which builds a fresh `Stream`
//!   that *captures* it by move. The callback therefore reads an immutable slice with no
//!   lock — realtime-safe.
//!
//! Every public method takes `&self`, so a single `PlaybackEngine` lives directly in a
//! Tauri `State` with no outer `Mutex`.
//!
//! ## Frame domains
//!
//! The public API speaks **source frames** — frames at the loaded buffer's own sample rate
//! (the internal 48 kHz material). Internally the cursor advances in **device frames**
//! (the rate the hardware actually runs at). [`src_to_device_frame`] /
//! [`device_to_src_frame`] convert at the boundary, so the UI's playhead maps straight
//! onto the peaks pyramid, which is also in source frames.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anvil_media::AudioBuffer;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::resample::resample_planar;
use crate::AudioError;

/// Convert a source-rate frame index to the device-rate cursor.
fn src_to_device_frame(src_frame: u64, src_rate: u32, device_rate: u32) -> u64 {
    if src_rate == 0 {
        return 0;
    }
    ((src_frame as u128 * device_rate as u128) / src_rate as u128) as u64
}

/// Convert a device-rate cursor back to a source-rate frame index (for the UI playhead).
fn device_to_src_frame(device_frame: u64, src_rate: u32, device_rate: u32) -> u64 {
    if device_rate == 0 {
        return 0;
    }
    ((device_frame as u128 * src_rate as u128) / device_rate as u128) as u64
}

/// Shared, lock-free transport state read by the audio callback and written by transport
/// calls. `device_frames`/`src_rate` are updated on each load and are stable while a track
/// plays.
#[derive(Debug)]
struct Transport {
    /// Playback cursor, in **device frames**.
    pos: AtomicU64,
    /// Total device frames in the current track (end of buffer).
    device_frames: AtomicU64,
    /// Sample rate of the loaded source buffer, for source/device frame conversion.
    src_rate: AtomicU32,
    /// Whether the callback should advance and emit audio.
    playing: AtomicBool,
    /// Output device rate — fixed for the engine's life.
    device_rate: u32,
    /// Output device channel count — fixed for the engine's life.
    device_channels: u16,
}

impl Transport {
    fn position_src(&self) -> u64 {
        let pos = self.pos.load(Ordering::Acquire);
        let src_rate = self.src_rate.load(Ordering::Acquire);
        device_to_src_frame(pos, src_rate, self.device_rate)
    }
}

/// A track resampled/interleaved to the device format, ready for the callback.
struct DeviceTrack {
    /// Interleaved samples, `device_channels` per frame, at the device rate.
    samples: Arc<Vec<f32>>,
}

enum AudioCmd {
    Load(DeviceTrack),
    Shutdown,
}

/// Live audio-thread machinery. Absent when no output device is available (headless CI,
/// no sound card) — the engine then degrades to inert transport rather than failing.
struct Live {
    transport: Arc<Transport>,
    cmd_tx: Sender<AudioCmd>,
    handle: Option<JoinHandle<()>>,
}

/// Owns cpal output and transport for one loaded source at a time.
pub struct PlaybackEngine {
    live: Option<Live>,
}

impl PlaybackEngine {
    /// Open the default output device and start the audio thread.
    ///
    /// Never errors on a missing device: if none is available it returns an engine whose
    /// transport is inert (`load`/`play` are no-ops, `position` stays 0), so the rest of
    /// the app and the unit tests run without a sound card.
    pub fn new() -> Self {
        match Self::spawn_live() {
            Ok(live) => Self { live: Some(live) },
            Err(e) => {
                tracing::warn!("no audio output device: {e}; playback disabled");
                Self { live: None }
            }
        }
    }

    fn spawn_live() -> Result<Live, AudioError> {
        // Probe the device on this thread so we know the format before the audio thread
        // needs it; only the non-`Send` `Stream` is confined to the audio thread.
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::NoOutputDevice)?;
        let supported = device
            .default_output_config()
            .map_err(|e| AudioError::Device(e.to_string()))?;
        let device_rate = supported.sample_rate().0;
        let device_channels = supported.channels();
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let transport = Arc::new(Transport {
            pos: AtomicU64::new(0),
            device_frames: AtomicU64::new(0),
            src_rate: AtomicU32::new(device_rate),
            playing: AtomicBool::new(false),
            device_rate,
            device_channels,
        });

        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        let thread_transport = transport.clone();
        let handle = std::thread::Builder::new()
            .name("anvil-audio".into())
            .spawn(move || {
                audio_thread(device, config, sample_format, thread_transport, cmd_rx);
            })
            .map_err(|e| AudioError::Device(e.to_string()))?;

        Ok(Live {
            transport,
            cmd_tx,
            handle: Some(handle),
        })
    }

    /// Load a source buffer for playback: resample and interleave it to the device format,
    /// hand it to the audio thread, and reset the transport to a paused start.
    ///
    /// No-op (returns `Ok`) when there is no output device.
    pub fn load(&self, buffer: &AudioBuffer) -> Result<(), AudioError> {
        let Some(live) = &self.live else {
            return Ok(());
        };
        let t = &live.transport;

        let src_rate = buffer.sample_rate();
        let interleaved = interleave_for_device(buffer, t.device_rate, t.device_channels);
        let device_frames = if t.device_channels == 0 {
            0
        } else {
            (interleaved.len() / t.device_channels as usize) as u64
        };

        // Publish track metadata before the callback can see the new samples.
        t.playing.store(false, Ordering::Release);
        t.pos.store(0, Ordering::Release);
        t.src_rate.store(src_rate, Ordering::Release);
        t.device_frames.store(device_frames, Ordering::Release);

        live.cmd_tx
            .send(AudioCmd::Load(DeviceTrack {
                samples: Arc::new(interleaved),
            }))
            .map_err(|_| AudioError::AudioThreadGone)?;
        Ok(())
    }

    /// Start (or resume) playback. If the cursor is at the end, restart from the top.
    pub fn play(&self) {
        let Some(live) = &self.live else { return };
        let t = &live.transport;
        let end = t.device_frames.load(Ordering::Acquire);
        if end == 0 {
            return;
        }
        if t.pos.load(Ordering::Acquire) >= end {
            t.pos.store(0, Ordering::Release);
        }
        t.playing.store(true, Ordering::Release);
    }

    /// Pause, leaving the cursor where it is.
    pub fn pause(&self) {
        if let Some(live) = &self.live {
            live.transport.playing.store(false, Ordering::Release);
        }
    }

    /// Stop and rewind to the start.
    pub fn stop(&self) {
        if let Some(live) = &self.live {
            live.transport.playing.store(false, Ordering::Release);
            live.transport.pos.store(0, Ordering::Release);
        }
    }

    /// Seek to `src_frame` (source-rate frames), clamped to the track length.
    pub fn seek(&self, src_frame: u64) {
        let Some(live) = &self.live else { return };
        let t = &live.transport;
        let src_rate = t.src_rate.load(Ordering::Acquire);
        let end = t.device_frames.load(Ordering::Acquire);
        let device_frame = src_to_device_frame(src_frame, src_rate, t.device_rate).min(end);
        t.pos.store(device_frame, Ordering::Release);
    }

    /// Current playback position in **source frames** (maps directly onto the waveform).
    pub fn position(&self) -> u64 {
        self.live
            .as_ref()
            .map_or(0, |live| live.transport.position_src())
    }

    /// Whether audio is currently advancing.
    pub fn is_playing(&self) -> bool {
        self.live
            .as_ref()
            .is_some_and(|live| live.transport.playing.load(Ordering::Acquire))
    }
}

impl Default for PlaybackEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        if let Some(live) = &mut self.live {
            let _ = live.cmd_tx.send(AudioCmd::Shutdown);
            if let Some(handle) = live.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Resample `buffer` to `device_rate` and interleave to `device_channels` (up/down-mixing
/// as needed): mono fans out to every device channel; extra device channels repeat the
/// last source channel; surplus source channels are dropped.
fn interleave_for_device(buffer: &AudioBuffer, device_rate: u32, device_channels: u16) -> Vec<f32> {
    let ch = device_channels as usize;
    if ch == 0 || buffer.frames() == 0 {
        return Vec::new();
    }
    let resampled = resample_planar(buffer.planar(), buffer.sample_rate(), device_rate);
    let src_ch = resampled.len();
    let frames = resampled.first().map_or(0, Vec::len);

    let mut out = vec![0.0f32; frames * ch];
    for f in 0..frames {
        for c in 0..ch {
            let sample = if src_ch == 0 {
                0.0
            } else if src_ch == 1 {
                resampled[0][f]
            } else {
                // Map device channel to a source channel; clamp extras to the last one.
                resampled[c.min(src_ch - 1)][f]
            };
            out[f * ch + c] = sample;
        }
    }
    out
}

/// Audio-thread entry point: owns the device, rebuilds the `Stream` on each `Load`, and
/// keeps it alive between commands so cpal keeps calling the callback.
fn audio_thread(
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
    transport: Arc<Transport>,
    cmd_rx: mpsc::Receiver<AudioCmd>,
) {
    let mut _stream: Option<cpal::Stream> = None;
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            AudioCmd::Load(track) => {
                match build_stream(&device, &config, sample_format, transport.clone(), track) {
                    Ok(stream) => {
                        if let Err(e) = stream.play() {
                            tracing::error!("failed to start output stream: {e}");
                        }
                        // Replace (and drop) any previous stream.
                        _stream = Some(stream);
                    }
                    Err(e) => tracing::error!("failed to build output stream: {e}"),
                }
            }
            AudioCmd::Shutdown => break,
        }
    }
    // `_stream` drops here, releasing the device.
}

/// Build a cpal output stream for the device's sample format, capturing the immutable
/// track by move so the callback reads it lock-free.
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    transport: Arc<Transport>,
    track: DeviceTrack,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    match sample_format {
        SampleFormat::F32 => build_stream_t::<f32>(device, config, transport, track),
        SampleFormat::I16 => build_stream_t::<i16>(device, config, transport, track),
        SampleFormat::U16 => build_stream_t::<u16>(device, config, transport, track),
        other => {
            tracing::warn!("unsupported sample format {other:?}; defaulting to f32");
            build_stream_t::<f32>(device, config, transport, track)
        }
    }
}

fn build_stream_t<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    transport: Arc<Transport>,
    track: DeviceTrack,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = transport.device_channels as usize;
    let samples = track.samples;
    let err_fn = |e| tracing::error!("output stream error: {e}");

    device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames_out = out.len().checked_div(channels).unwrap_or(0);
            let playing = transport.playing.load(Ordering::Acquire);
            let total = transport.device_frames.load(Ordering::Acquire);

            if !playing || samples.is_empty() || channels == 0 {
                for s in out.iter_mut() {
                    *s = T::EQUILIBRIUM;
                }
                return;
            }

            let mut pos = transport.pos.load(Ordering::Acquire);
            for f in 0..frames_out {
                if pos >= total {
                    for c in 0..channels {
                        out[f * channels + c] = T::EQUILIBRIUM;
                    }
                    continue;
                }
                let base = pos as usize * channels;
                for c in 0..channels {
                    out[f * channels + c] = T::from_sample(samples[base + c]);
                }
                pos += 1;
            }

            let pos = pos.min(total);
            transport.pos.store(pos, Ordering::Release);
            if pos >= total {
                // Reached the end — auto-pause so is_playing() reflects reality.
                transport.playing.store(false, Ordering::Release);
            }
        },
        err_fn,
        Some(Duration::from_millis(200)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_conversion_is_inverse_at_matching_rates() {
        for f in [0u64, 1, 480, 48_000, 1_000_000] {
            assert_eq!(src_to_device_frame(f, 48_000, 48_000), f);
            assert_eq!(device_to_src_frame(f, 48_000, 48_000), f);
        }
    }

    #[test]
    fn upsampling_device_scales_cursor() {
        // 48 kHz source on a 96 kHz device: one source frame = two device frames.
        assert_eq!(src_to_device_frame(100, 48_000, 96_000), 200);
        assert_eq!(device_to_src_frame(200, 48_000, 96_000), 100);
    }

    #[test]
    fn downsampling_device_scales_cursor() {
        // 48 kHz source on a 44.1 kHz device.
        let dev = src_to_device_frame(48_000, 48_000, 44_100);
        assert_eq!(dev, 44_100);
        assert_eq!(device_to_src_frame(44_100, 48_000, 44_100), 48_000);
    }

    #[test]
    fn conversion_handles_zero_rates_without_panicking() {
        assert_eq!(src_to_device_frame(1_000, 0, 48_000), 0);
        assert_eq!(device_to_src_frame(1_000, 48_000, 0), 0);
    }

    #[test]
    fn no_overflow_on_long_files() {
        // ~3 hours at 48 kHz is > 500M frames; the u128 intermediate must not overflow.
        let three_hours = 48_000u64 * 60 * 60 * 3;
        let dev = src_to_device_frame(three_hours, 48_000, 96_000);
        assert_eq!(dev, three_hours * 2);
    }

    #[test]
    fn mono_interleaves_to_stereo_device() {
        let buf = AudioBuffer::from_planar(vec![vec![0.25, 0.5, 0.75]], 48_000);
        let out = interleave_for_device(&buf, 48_000, 2);
        // Each mono sample fans out to both channels.
        assert_eq!(out, vec![0.25, 0.25, 0.5, 0.5, 0.75, 0.75]);
    }

    #[test]
    fn stereo_interleaves_frame_major() {
        let buf = AudioBuffer::from_planar(vec![vec![0.1, 0.2], vec![-0.1, -0.2]], 48_000);
        let out = interleave_for_device(&buf, 48_000, 2);
        assert_eq!(out, vec![0.1, -0.1, 0.2, -0.2]);
    }

    #[test]
    fn zero_channel_device_yields_empty() {
        let buf = AudioBuffer::from_planar(vec![vec![0.1, 0.2]], 48_000);
        assert!(interleave_for_device(&buf, 48_000, 0).is_empty());
    }
}
