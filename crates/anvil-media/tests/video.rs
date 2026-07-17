//! Video demux/remux tests (M2): the video stream must survive a remux byte-for-byte
//! (`-c:v copy`), while the audio track is replaced with a (simulated) mastered buffer.
//!
//! Building and inspecting a video fixture both need ffmpeg (nothing else in this crate can
//! produce or introspect a video container), so this whole file gates on
//! [`FfmpegSidecar::locate`] and skips cleanly when it's absent.

use std::path::{Path, PathBuf};
use std::process::Command;

use anvil_media::{extract_audio, remux_with_audio, FfmpegSidecar};

fn tmp_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// A short synthetic video (color-bar pattern + a sine tone) built entirely by ffmpeg's
/// `lavfi` test sources — no binary fixtures in the repo. Returns `false` (rather than
/// panicking) if none of the candidate encoders are available, so the caller can skip cleanly.
///
/// The encoder is deliberately **not** libx264: that is a GPL component absent from the LGPL
/// build Cleanroom ships (see `sidecar::FFMPEG_PINS`), so a fixture that required it would only ever
/// run on a developer's GPL ffmpeg and silently skip against the real shipping binary. We build
/// the source with an LGPL/OS **H.264** encoder instead — `libopenh264`, then Media Foundation
/// (Windows), then VideoToolbox (macOS) — so the remux path is exercised on the build that
/// actually ships. The candidates are H.264-only because the test asserts the stream survives
/// remux as h264; a build with none of them skips cleanly rather than faking the fixture with
/// another codec. (Remux itself only stream-*copies* video; the encoder is a fixture detail.)
fn make_test_video(sidecar: &FfmpegSidecar, path: &Path) -> bool {
    for vcodec in ["libopenh264", "h264_mf", "h264_videotoolbox"] {
        let status = Command::new(sidecar.binary())
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
            ])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-c:v", vcodec, "-c:a", "aac", "-shortest"])
            .arg(path)
            .status();
        if matches!(status, Ok(s) if s.success()) && path.is_file() {
            return true;
        }
    }
    false
}

/// The human-readable `ffmpeg -i` banner for `path` (stderr), used to eyeball codec/stream
/// facts the same way `tests/decode.rs`'s fixture helpers do.
fn banner(sidecar: &FfmpegSidecar, path: &Path) -> String {
    let output = Command::new(sidecar.binary())
        .args(["-hide_banner", "-i"])
        .arg(path)
        .output()
        .expect("run ffmpeg -i");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn remux_copies_video_and_replaces_audio() {
    let Ok(sidecar) = FfmpegSidecar::locate() else {
        eprintln!("skipping remux_copies_video_and_replaces_audio: ffmpeg unavailable");
        return;
    };
    let video_path = tmp_dir().join("remux_src.mp4");
    if !make_test_video(&sidecar, &video_path) {
        eprintln!(
            "skipping remux_copies_video_and_replaces_audio: no usable LGPL/OS video encoder \
             in this ffmpeg build to synthesize the source clip"
        );
        return;
    }

    let audio = extract_audio(&video_path).expect("extract audio from video");
    assert!(audio.frames() > 0, "extracted audio should not be empty");

    // Simulate mastering with a trivial gain change, so the remuxed audio is provably not a
    // byte-identical passthrough of the original track.
    let mut mastered = audio.clone();
    for channel in mastered.planar_mut() {
        for sample in channel.iter_mut() {
            *sample *= 0.5;
        }
    }

    let out_path = tmp_dir().join("remuxed.mp4");
    remux_with_audio(&sidecar, &video_path, &mastered, &out_path)
        .expect("remux with mastered audio");
    assert!(out_path.is_file());

    let before = banner(&sidecar, &video_path);
    let after = banner(&sidecar, &out_path);
    assert!(
        before.contains("Video: h264"),
        "source fixture should be h264:\n{before}"
    );
    assert!(
        after.contains("Video: h264"),
        "remuxed video stream should stay h264 (-c:v copy, never re-encoded):\n{after}"
    );
    assert!(
        after.contains("Audio: aac"),
        "remuxed audio should be (re-)encoded to the mp4 default (aac):\n{after}"
    );

    // The remuxed audio, decoded back, should carry real signal (not silence/garbage).
    let remuxed_audio = extract_audio(&out_path).expect("decode remuxed audio back");
    let rms: f32 = {
        let ch = remuxed_audio.channel(0);
        (ch.iter().map(|s| s * s).sum::<f32>() / ch.len().max(1) as f32).sqrt()
    };
    assert!(rms > 0.02, "remuxed audio should not be silent, rms={rms}");
}
