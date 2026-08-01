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

// ---------------------------------------------------------------------------------------------
// Cut-word split: per-part video export (handoff/09 §6)
// ---------------------------------------------------------------------------------------------

/// Whether `path`'s extension is a video container this crate demuxes/remuxes. Used by the
/// split path to decide between the audio-only and video export routes (09 §1).
pub fn is_video_container(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "mov" | "m4v" | "mkv" | "webm")
    )
}

/// The bits of the source video stream the segment encoder wants: canvas + rate for the
/// bitrate heuristic. Parsed from the `ffmpeg -i` banner ([`parse_video_banner`]) — a
/// best-effort probe; every field may be absent on exotic containers, and the encoder
/// falls back to a generous default.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// Frames per second as reported (`0.0` when the banner carries no fps).
    pub fps: f64,
    /// Stream (or container) bitrate in kb/s, when reported.
    pub bit_rate_kbps: Option<u32>,
}

/// Probe the first video stream of `path` via the sidecar's `-i` banner (the same
/// [`FfmpegSidecar::banner`] read `probe`/`read_chapters` share). `None` when the file has
/// no video stream or the banner defies parsing — callers fall back to defaults.
pub fn probe_video(sidecar: &FfmpegSidecar, path: &Path) -> Option<VideoInfo> {
    parse_video_banner(&sidecar.banner(path).ok()?)
}

/// Parse the first `Video:` stream line of an `ffmpeg -i` banner. Pure and deterministic —
/// the testable half of [`probe_video`].
pub(crate) fn parse_video_banner(banner: &str) -> Option<VideoInfo> {
    let line = banner
        .lines()
        .find(|l| l.contains("Video:") && !l.contains("attached pic"))?;
    let mut width = None;
    let mut fps = 0.0_f64;
    let mut bit_rate_kbps = None;
    for token in line.split(',').map(str::trim) {
        // "1920x1080" or "1920x1080 [SAR 1:1 DAR 16:9]" — dimensions are the first word.
        if width.is_none() {
            if let Some((w, h)) = token
                .split_whitespace()
                .next()
                .and_then(|t| t.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            {
                if w > 0 && h > 0 {
                    width = Some((w, h));
                    continue;
                }
            }
        }
        if let Some(rate) = token.strip_suffix("fps").map(str::trim) {
            fps = rate.parse().unwrap_or(0.0);
        } else if let Some(kbps) = token.strip_suffix("kb/s").map(str::trim) {
            bit_rate_kbps = kbps.parse::<f64>().ok().map(|k| k.round() as u32);
        }
    }
    let (width, height) = width?;
    Some(VideoInfo {
        width,
        height,
        fps,
        bit_rate_kbps,
    })
}

/// Segment video bitrate (09 §6): `max(source × 1.2, 0.1 bits-per-pixel)` clamped to
/// `[2, 20] Mb/s`; a generous 8 Mb/s when the probe came up empty. The 0.1 bpp floor is the
/// same rule of thumb behind [`crate::clip::Aspect`]'s 6/8 Mb/s canvas constants.
pub(crate) fn segment_video_bitrate_kbps(info: Option<&VideoInfo>) -> u32 {
    let Some(info) = info else { return 8_000 };
    let fps = if info.fps > 0.0 { info.fps } else { 30.0 };
    let bpp_kbps = (f64::from(info.width) * f64::from(info.height) * fps * 0.1 / 1000.0) as u32;
    let scaled_src = info.bit_rate_kbps.map(|b| b.saturating_mul(12) / 10);
    scaled_src.unwrap_or(0).max(bpp_kbps).clamp(2_000, 20_000)
}

/// Export one split part of a video source (09 §6): frame-accurate seek into `video_in`,
/// video **re-encoded** through the LGPL H.264 ladder ([`FfmpegSidecar::h264_encoder`] —
/// stream copy cannot cut off-keyframe, and a boundary that snaps to a keyframe would clip
/// speech or leave the cut word in), the part's mastered audio muxed from `pipe:0` exactly
/// like [`remux_with_audio_spec`]. The audio codec follows `out`'s container. HEVC/10-bit
/// sources land as 8-bit H.264 (`yuv420p`) — the broadly-playable v1 target.
pub fn export_video_segment(
    sidecar: &FfmpegSidecar,
    video_in: &Path,
    start_secs: f64,
    duration_secs: f64,
    mastered_audio: &AudioBuffer,
    out: &Path,
    progress: impl FnMut(f32),
) -> Result<(), MediaError> {
    if duration_secs <= 0.0 {
        return Err(MediaError::InvalidClip(format!(
            "segment duration must be positive, got {duration_secs}"
        )));
    }
    let encoder = sidecar.h264_encoder()?;
    let bitrate = segment_video_bitrate_kbps(probe_video(sidecar, video_in).as_ref());
    let mut cmd = build_segment_command(
        sidecar.binary(),
        video_in,
        start_secs,
        duration_secs,
        mastered_audio,
        out,
        &encoder,
        bitrate,
    );
    let child = cmd.spawn().map_err(MediaError::from)?;
    run_encode_child(child, mastered_audio, progress)
}

/// The segment export's ffmpeg invocation — split out (and taking the bare binary path) so
/// the argument contract (input seek before `-i`, LGPL encoder, faststart only on
/// MP4-family) is unit-testable without a pinned ffmpeg binary on the machine.
#[allow(clippy::too_many_arguments)]
fn build_segment_command(
    ffmpeg: &Path,
    video_in: &Path,
    start_secs: f64,
    duration_secs: f64,
    mastered_audio: &AudioBuffer,
    out: &Path,
    encoder: &str,
    bitrate_kbps: u32,
) -> Command {
    let channels = mastered_audio.channel_count().max(1);
    let audio_spec = OutputSpec::new(default_audio_format_for_container(out));

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
        // Input seek (before `-i`): frame-accurate *because* we re-encode — ffmpeg decodes
        // from the previous keyframe and discards up to the exact target.
        .args(["-ss", &format!("{start_secs:.4}")])
        .arg("-i")
        .arg(video_in)
        .args(["-f", "f32le"])
        .args(["-ar", &mastered_audio.sample_rate().to_string()])
        .args(["-ac", &channels.to_string()])
        .arg("-i")
        .arg("pipe:0")
        .args(["-map", "0:v:0", "-map", "1:a:0"])
        .args(["-c:v", encoder])
        .args(["-b:v", &format!("{bitrate_kbps}k")])
        .args(["-pix_fmt", "yuv420p"]);

    apply_output_spec(&mut cmd, &audio_spec);

    let is_mp4_family = matches!(
        out.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "mov" | "m4v") | None
    );
    if is_mp4_family {
        cmd.args(["-movflags", "+faststart"]);
    }

    cmd.args(["-t", &format!("{duration_secs:.4}")])
        .args(["-shortest"])
        .args(["-progress", "pipe:2"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real-world `ffmpeg -i` banner line (phone mp4).
    const BANNER: &str = "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), \
yuv420p(progressive), 1920x1080 [SAR 1:1 DAR 16:9], 7982 kb/s, 29.97 fps, 29.97 tbr, \
30k tbn (default)\n  Stream #0:1[0x2](und): Audio: aac (LC), 48000 Hz, stereo, fltp, 192 kb/s";

    #[test]
    fn banner_parse_extracts_dims_fps_bitrate() {
        let info = parse_video_banner(BANNER).expect("parses");
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert!((info.fps - 29.97).abs() < 1e-9);
        assert_eq!(info.bit_rate_kbps, Some(7982));
    }

    #[test]
    fn banner_parse_survives_missing_fields_and_no_video() {
        let audio_only = "  Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 192 kb/s";
        assert_eq!(parse_video_banner(audio_only), None);
        let sparse = "  Stream #0:0: Video: vp9, yuv420p, 640x360, 25 tbr";
        let info = parse_video_banner(sparse).expect("dims alone are enough");
        assert_eq!((info.width, info.height), (640, 360));
        assert_eq!(info.fps, 0.0);
        assert_eq!(info.bit_rate_kbps, None);
    }

    #[test]
    fn bitrate_rule_scales_and_clamps() {
        // No probe → generous default.
        assert_eq!(segment_video_bitrate_kbps(None), 8_000);
        // 1080p29.97 at 7982 kb/s source → 1.2× source wins (9578) over 0.1 bpp (6213).
        let hd = VideoInfo {
            width: 1920,
            height: 1080,
            fps: 29.97,
            bit_rate_kbps: Some(7_982),
        };
        assert_eq!(segment_video_bitrate_kbps(Some(&hd)), 9_578);
        // Tiny low-rate source clamps up to the 2 Mb/s floor.
        let tiny = VideoInfo {
            width: 320,
            height: 240,
            fps: 15.0,
            bit_rate_kbps: Some(300),
        };
        assert_eq!(segment_video_bitrate_kbps(Some(&tiny)), 2_000);
        // Absurd source clamps down to 20 Mb/s.
        let huge = VideoInfo {
            width: 3840,
            height: 2160,
            fps: 60.0,
            bit_rate_kbps: Some(80_000),
        };
        assert_eq!(segment_video_bitrate_kbps(Some(&huge)), 20_000);
    }

    #[test]
    fn video_container_detection() {
        for ext in ["mp4", "mov", "m4v", "mkv", "webm", "MP4"] {
            assert!(is_video_container(Path::new(&format!("a.{ext}"))), "{ext}");
        }
        for ext in ["wav", "mp3", "m4a", "flac"] {
            assert!(!is_video_container(Path::new(&format!("a.{ext}"))), "{ext}");
        }
    }

    /// The argument contract (09 §6): `-ss` is an *input* option on the video leg (appears
    /// before the first `-i`), the H.264 encoder is the one passed (never GPL), audio maps
    /// from the pipe, and `+faststart` appears only for MP4-family outputs.
    #[test]
    fn segment_command_argument_contract() {
        let audio = AudioBuffer::silence(2, 48_000, 48_000);
        let args_for = |out: &str| -> Vec<String> {
            build_segment_command(
                Path::new("ffmpeg"),
                Path::new("in.mp4"),
                12.5,
                30.0,
                &audio,
                Path::new(out),
                "h264_mf",
                9_000,
            )
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };

        let args = args_for("out.mp4");
        let ss = args.iter().position(|a| a == "-ss").expect("-ss present");
        let first_i = args.iter().position(|a| a == "-i").expect("-i present");
        assert!(ss < first_i, "-ss must be an input option: {args:?}");
        assert_eq!(args[ss + 1], "12.5000");
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "h264_mf");
        assert!(args.iter().any(|a| a == "9000k"));
        assert!(args.iter().any(|a| a == "+faststart"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "1:a:0"));
        assert!(!args.iter().any(|a| crate::clip::is_gpl_video_encoder(a)));

        // mkv: no faststart (mp4-only muxer flag), FLAC audio per container default.
        let args = args_for("out.mkv");
        assert!(!args.iter().any(|a| a == "+faststart"));
        assert!(args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "flac"));
    }
}
