//! # anvil-media
//!
//! Media IO (ADR-005): decode (symphonia + ffmpeg-sidecar fallback), encode (ffmpeg
//! sidecar), metadata/chapters (lofty + ffmpeg ffmetadata), and video demux/remux. Decoders
//! landed in M0 lane B; encoders, metadata, and video land in M2 lane A. M4 adds [`clip`] —
//! Clip Studio's render engine (a time range + word timestamps → a captioned social MP4).
//!
//! This module fixes the engine's internal representation: **planar (per-channel,
//! non-interleaved) 32-bit float at 48 kHz** (ADR-002). Decoders normalize into
//! [`AudioBuffer`]; every DSP module and encoder consumes it. Streaming code holds only
//! one block at a time (a 3-hour file never fully resides in RAM).

use anvil_core::INTERNAL_SAMPLE_RATE;

pub mod clip;
pub mod decode;
pub mod encode;
pub mod encode_stream;
pub mod error;
pub mod metadata;
pub mod probe;
pub mod sidecar;
pub mod video;

pub use clip::{
    caption_cues, caption_script, is_gpl_video_encoder, render_clip, render_clip_with_progress,
    Aspect, Background, CaptionCue, CaptionStyle, ClipSpec, ClipWord, CLIP_FPS, GPL_VIDEO_ENCODERS,
    LGPL_H264_ENCODERS,
};
pub use decode::{decode_blocks, decode_to_buffer, BlockDecoder};
pub use encode::{
    encode, encode_m4b_audiobook, encode_multi, encode_with_progress, OutputFormat, OutputSpec,
};
pub use encode_stream::StreamEncoder;
pub use error::MediaError;
pub use metadata::{read_chapters, write_chapters, Chapter, CoverArt, TagEditor};
pub use probe::{probe, MediaInfo};
pub use sidecar::{
    content_pinned_sha256, current_pin, gpl_markers_in, macho_content_sha256, pinned_sha256,
    sha256_file, FfmpegPin, FfmpegSidecar, FFMPEG_PINS, GPL_CONFIGURE_MARKERS,
};
pub use video::{extract_audio, extract_audio_blocks, remux_with_audio, remux_with_audio_spec};

/// Planar 32-bit float audio. `channels[c][frame]` — outer index is the channel, inner
/// is the sample. All channels share the same length ([`AudioBuffer::frames`]).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBuffer {
    channels: Vec<Vec<f32>>,
    sample_rate: u32,
}

impl AudioBuffer {
    /// An empty buffer with `channels` channels at `sample_rate`.
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        Self {
            channels: vec![Vec::new(); channels],
            sample_rate,
        }
    }

    /// An empty buffer at the engine's internal rate ([`INTERNAL_SAMPLE_RATE`]).
    pub fn new_internal(channels: usize) -> Self {
        Self::new(channels, INTERNAL_SAMPLE_RATE)
    }

    /// Build from existing planar data. All channels should have equal length.
    pub fn from_planar(channels: Vec<Vec<f32>>, sample_rate: u32) -> Self {
        debug_assert!(
            channels.windows(2).all(|w| w[0].len() == w[1].len()),
            "AudioBuffer channels must be equal length"
        );
        Self {
            channels,
            sample_rate,
        }
    }

    /// A silent buffer of `frames` samples per channel.
    pub fn silence(channels: usize, frames: usize, sample_rate: u32) -> Self {
        Self {
            channels: vec![vec![0.0; frames]; channels],
            sample_rate,
        }
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Number of sample frames (length of channel 0, or 0 if channel-less).
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn is_empty(&self) -> bool {
        self.frames() == 0
    }

    pub fn channel(&self, index: usize) -> &[f32] {
        &self.channels[index]
    }

    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        &mut self.channels[index]
    }

    /// All channels as planar slices.
    pub fn planar(&self) -> &[Vec<f32>] {
        &self.channels
    }

    /// All channels, mutably — DSP processors iterate this.
    pub fn planar_mut(&mut self) -> &mut [Vec<f32>] {
        &mut self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_has_expected_shape() {
        let b = AudioBuffer::silence(2, 480, 48_000);
        assert_eq!(b.channel_count(), 2);
        assert_eq!(b.frames(), 480);
        assert!(b.channel(0).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn internal_buffer_uses_internal_rate() {
        assert_eq!(
            AudioBuffer::new_internal(1).sample_rate(),
            INTERNAL_SAMPLE_RATE
        );
    }
}
