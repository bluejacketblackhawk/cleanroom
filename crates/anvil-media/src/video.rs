//! Video demux → remux (ADR-005 §Video): pull the audio track out of a video container for
//! mastering, then mux the mastered result back in with the **video stream copied**, never
//! re-encoded (`-c:v copy`, always — this module has no code path that touches video frames).
//!
//! The demux half is nothing new: [`extract_audio`]/[`extract_audio_blocks`] are the existing
//! [`crate::decode`] entry points (which already select the ffmpeg sidecar for mkv/webm/mov —
//! symphonia doesn't demux those containers — and always `-map 0:a:0 -vn` to skip video). This
//! module only adds the remux-back half.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::decode::{decode_blocks, decode_to_buffer, BlockDecoder};
use crate::encode::{apply_output_spec, run_encode_child, OutputFormat, OutputSpec};
use crate::error::MediaError;
use crate::sidecar::FfmpegSidecar;
use crate::AudioBuffer;

/// Extract the audio track of a video (or any media) file into one [`AudioBuffer`]. A thin
/// alias over [`decode_to_buffer`] — video containers already route through the ffmpeg
/// sidecar there, which discards the video stream (`-vn`).
pub fn extract_audio(path: &Path) -> Result<AudioBuffer, MediaError> {
    decode_to_buffer(path)
}

/// Streaming form of [`extract_audio`] for long recordings — see [`decode_blocks`].
pub fn extract_audio_blocks(path: &Path) -> Result<BlockDecoder, MediaError> {
    decode_blocks(path)
}

/// Mux `mastered_audio` into a copy of `video_in`'s video stream, writing `out`. The video
/// stream is always `-c:v copy` (never re-encoded, per ADR-005); the audio codec is chosen
/// from `out`'s extension: AAC for mp4/mov/m4v, FLAC for mkv, Opus for webm (mkv/webm can't
/// carry AAC's patent-bearing bitstream the same way mp4 does, and webm specifically only
/// accepts Vorbis/Opus). Use [`remux_with_audio_spec`] to pick the codec explicitly.
pub fn remux_with_audio(
    sidecar: &FfmpegSidecar,
    video_in: &Path,
    mastered_audio: &AudioBuffer,
    out: &Path,
) -> Result<(), MediaError> {
    let spec = OutputSpec::new(default_audio_format_for_container(out));
    remux_with_audio_spec(sidecar, video_in, mastered_audio, out, &spec)
}

/// [`remux_with_audio`] with an explicit audio [`OutputSpec`] (codec/bitrate/mono/etc.)
/// instead of the container-based default.
pub fn remux_with_audio_spec(
    sidecar: &FfmpegSidecar,
    video_in: &Path,
    mastered_audio: &AudioBuffer,
    out: &Path,
    audio_spec: &OutputSpec,
) -> Result<(), MediaError> {
    let channels = mastered_audio.channel_count().max(1);

    let mut cmd = Command::new(sidecar.binary());
    cmd.args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(video_in)
        .args(["-f", "f32le"])
        .args(["-ar", &mastered_audio.sample_rate().to_string()])
        .args(["-ac", &channels.to_string()])
        .arg("-i")
        .arg("pipe:0")
        .args(["-map", "0:v:0", "-map", "1:a:0", "-shortest"])
        .args(["-c:v", "copy"]);

    apply_output_spec(&mut cmd, audio_spec);

    cmd.args(["-progress", "pipe:2"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let child = cmd.spawn().map_err(MediaError::from)?;
    run_encode_child(child, mastered_audio, |_| {})
}

fn default_audio_format_for_container(out: &Path) -> OutputFormat {
    match out
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("webm") => OutputFormat::Opus,
        Some("mkv") => OutputFormat::Flac,
        // mp4/mov/m4v and anything unrecognized: AAC is the broadly-compatible default.
        _ => OutputFormat::Aac,
    }
}
