//! Cheap metadata probe: sample rate, channel count, duration, and a format label — read
//! from container headers without decoding the whole file.
//!
//! symphonia is tried first; anything it can't identify falls back to the ffmpeg sidecar
//! (see [`crate::sidecar`]), matching the decode fallback chain so `probe` and `decode`
//! agree on which backend owns a given file.

use std::path::Path;

use serde::{Deserialize, Serialize};
use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::formats::{FormatOptions, Track};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::MediaError;
use crate::sidecar::FfmpegSidecar;

/// Container/stream facts needed by the CLI and playback layers before decoding.
///
/// `source_sample_rate` is the file's *native* rate; decoding always normalizes to
/// [`anvil_core::INTERNAL_SAMPLE_RATE`], so this is only informational (and drives the
/// resampler ratio internally).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Total playing time in seconds. `0.0` when the container does not record a frame
    /// count without a full scan (e.g. some CBR MP3s); documented as best-effort.
    pub duration_secs: f64,
    /// Native sample rate of the source audio track, in Hz.
    pub source_sample_rate: u32,
    /// Channel count of the source audio track.
    pub channels: u16,
    /// Short format label — the file extension when present, else the codec name.
    pub format: String,
}

/// Probe `path` for [`MediaInfo`]. Tries symphonia, then the ffmpeg sidecar for containers
/// symphonia does not support (mkv/webm/mov and other video wrappers).
pub fn probe(path: &Path) -> Result<MediaInfo, MediaError> {
    match probe_symphonia(path) {
        Err(MediaError::UnsupportedFormat(_)) | Err(MediaError::NoAudioTrack(_)) => {
            FfmpegSidecar::locate()?.probe(path)
        }
        other => other,
    }
}

/// Pick the audio track we will report on / decode: the first track with a real codec and,
/// preferably, a known sample rate.
pub(crate) fn pick_audio_track(tracks: &[Track]) -> Option<&Track> {
    tracks
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some())
        .or_else(|| {
            tracks
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        })
}

fn probe_symphonia(path: &Path) -> Result<MediaInfo, MediaError> {
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
    let params = &track.codec_params;

    let source_sample_rate = params
        .sample_rate
        .ok_or_else(|| MediaError::UnsupportedFormat("audio track has no sample rate".into()))?;
    let channels = params.channels.map_or(0, |c| c.count()) as u16;

    // Use the exact frame count when the demuxer knows it (wav/flac/most mp4). Otherwise
    // report 0.0 rather than scanning the whole file for a duration — documented as
    // best-effort on the field itself.
    let duration_secs = params
        .n_frames
        .map_or(0.0, |n| n as f64 / source_sample_rate as f64);

    let format_label = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| {
            symphonia::default::get_codecs()
                .get_codec(params.codec)
                .map(|d| d.short_name.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    Ok(MediaInfo {
        duration_secs,
        source_sample_rate,
        channels,
        format: format_label,
    })
}
