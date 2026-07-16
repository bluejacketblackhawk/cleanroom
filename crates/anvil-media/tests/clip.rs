//! Clip Studio render tests (M4, feature P19).
//!
//! The caption *schedule* is proved hermetically in `src/clip.rs`'s unit tests (no ffmpeg): the
//! ASS script is deterministic bytes, so its timestamps can be diffed against the word timestamps
//! directly. This file proves the other half — that ffmpeg actually produces the MP4 we claim, and
//! that the captions really are **burned into the pixels** at the times the script asked for.
//!
//! Everything here needs ffmpeg (nothing else in the crate can write or introspect an MP4), so it
//! gates on [`FfmpegSidecar::locate`] and skips cleanly when it's absent — same as `tests/video.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anvil_media::{
    caption_cues, render_clip, Aspect, AudioBuffer, Background, CaptionStyle, ClipSpec, ClipWord,
    FfmpegSidecar, CLIP_FPS,
};

const SR: u32 = 48_000;
/// The 04 acceptance budget: captions land within ±1 frame of the word timestamps.
const FRAME: f64 = 1.0 / CLIP_FPS as f64;

fn tmp_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// A stereo sine of `secs` seconds — stands in for the mastered episode audio.
fn tone(secs: f64) -> AudioBuffer {
    let frames = (secs * f64::from(SR)) as usize;
    let channel: Vec<f32> = (0..frames)
        .map(|i| (i as f32 / SR as f32 * 220.0 * std::f32::consts::TAU).sin() * 0.4)
        .collect();
    AudioBuffer::from_planar(vec![channel.clone(), channel], SR)
}

/// Words on the *episode* timeline. The clip starts at 5.0 s, and the first word is deliberately
/// 0.5 s into it, so there is a caption-free lead-in the pixel probe can use as a baseline.
fn words() -> Vec<ClipWord> {
    let script = [
        "everything",
        "here",
        "runs",
        "on",
        "your",
        "own",
        "machine",
        "and",
        "it",
        "always",
        "will",
        "do",
    ];
    script
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let start = 5.5 + i as f64 * 0.4;
            ClipWord::new(*w, start, start + 0.3)
        })
        .collect()
}

fn banner(sidecar: &FfmpegSidecar, path: &Path) -> String {
    let output = Command::new(sidecar.binary())
        .args(["-hide_banner", "-i"])
        .arg(path)
        .output()
        .expect("run ffmpeg -i");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Container duration in seconds, parsed from the `-i` banner's `Duration: HH:MM:SS.ss`.
fn duration_secs(banner: &str) -> f64 {
    let token = banner
        .lines()
        .find_map(|l| l.trim().strip_prefix("Duration:"))
        .and_then(|rest| rest.split(',').next())
        .expect("banner carries a Duration")
        .trim()
        .to_string();
    let mut parts = token.split(':');
    let h: f64 = parts.next().unwrap().parse().unwrap();
    let m: f64 = parts.next().unwrap().parse().unwrap();
    let s: f64 = parts.next().unwrap().parse().unwrap();
    h * 3600.0 + m * 60.0 + s
}

/// Mean luminance of **frame number `n`** (not "the frame near time t" — a timestamp seek is
/// key-frame-fuzzy, and the whole point here is to pin caption onset to an exact frame). The
/// `select` filter picks the frame by index; captions are bright type on a black canvas, so this
/// jumps the instant one is on screen.
fn frame_brightness(sidecar: &FfmpegSidecar, path: &Path, n: i64) -> f64 {
    let output = Command::new(sidecar.binary())
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(path)
        .args(["-vf", &format!("select=eq(n\\,{n})"), "-vsync", "0"])
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .output()
        .expect("run ffmpeg frame probe");
    let pixels = output.stdout;
    assert!(!pixels.is_empty(), "no frame decoded at index {n}");
    pixels.iter().map(|&p| f64::from(p)).sum::<f64>() / pixels.len() as f64
}

#[test]
fn renders_an_mp4_with_burned_in_captions_video_and_mastered_audio() {
    let Ok(sidecar) = FfmpegSidecar::locate() else {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    };

    let audio = tone(20.0);
    let words = words();
    let spec = ClipSpec::new(5.0, 12.0)
        .with_aspect(Aspect::Vertical)
        .with_caption_style(CaptionStyle::Bold)
        .with_background(Background::solid("#101014"));
    let out = tmp_dir().join("clip_vertical.mp4");

    let mut last_progress = 0.0f32;
    anvil_media::render_clip_with_progress(&audio, &words, &spec, &out, |p| last_progress = p)
        .expect("render clip");

    assert!(out.is_file(), "the render should leave an mp4 behind");
    assert_eq!(last_progress, 1.0, "progress should finish at 1.0");

    let banner = banner(&sidecar, &out);
    assert!(
        banner.contains("Video: h264"),
        "video stream missing:\n{banner}"
    );
    assert!(
        banner.contains("Audio: aac"),
        "audio stream missing:\n{banner}"
    );
    assert!(
        banner.contains("1080x1920"),
        "9:16 canvas expected:\n{banner}"
    );
    assert!(
        !banner.contains("Video: hevc") && !banner.contains("mpeg4"),
        "the clip must be H.264:\n{banner}"
    );

    let duration = duration_secs(&banner);
    assert!(
        (duration - 7.0).abs() < 0.1,
        "clip [5, 12] should be 7 s, container says {duration} s"
    );

    // The mastered audio really is in there (not silence, not a dropped track).
    let decoded = anvil_media::decode_to_buffer(&out).expect("decode the clip back");
    let ch = decoded.channel(0);
    let rms = (ch.iter().map(|s| s * s).sum::<f32>() / ch.len().max(1) as f32).sqrt();
    assert!(
        rms > 0.05,
        "the clip's audio should carry the tone, rms={rms}"
    );
}

#[test]
fn burned_in_captions_appear_within_one_frame_of_the_word() {
    let Ok(sidecar) = FfmpegSidecar::locate() else {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    };

    let audio = tone(20.0);
    let words = words();
    // No title, flat near-black background: any brightness on screen is a caption.
    let spec = ClipSpec::new(5.0, 12.0)
        .with_aspect(Aspect::Square)
        .with_caption_style(CaptionStyle::Bold)
        .with_background(Background::solid("#000000"));
    let out = tmp_dir().join("clip_timing.mp4");
    render_clip(&audio, &words, &spec, &out).expect("render clip");

    let cues = caption_cues(&words, &spec);
    let first = cues.first().expect("the clip has captions");
    assert!(
        (first.start - 0.5).abs() < 1e-9,
        "first word is 0.5 s into the clip, cue says {}",
        first.start
    );

    // The frame the first word *should* light up on, and an empty lead-in frame to compare with.
    let word_frame = (first.start / FRAME).round() as i64;
    let empty = frame_brightness(&sidecar, &out, 2);
    let lit_threshold = empty + 0.5;

    // Walk the frames around the word and find where the caption actually switches on.
    let onset = ((word_frame - 5)..=(word_frame + 5))
        .find(|&n| frame_brightness(&sidecar, &out, n) > lit_threshold)
        .unwrap_or_else(|| {
            panic!("no caption ever appeared around frame {word_frame} (empty luma {empty})")
        });

    eprintln!(
        "caption onset: frame {onset}, word at frame {word_frame} \
         ({} frame error; empty luma {empty:.2}, lit luma {:.2})",
        (onset - word_frame).abs(),
        frame_brightness(&sidecar, &out, onset)
    );
    assert!(
        (onset - word_frame).abs() <= 1,
        "04 acceptance: the burned-in caption must land within ±1 frame of the word timestamp \
         — it lit up on frame {onset}, the word starts on frame {word_frame}"
    );

    // …and it clears again once the last cue has held out.
    let last = cues.last().unwrap();
    let after = ((last.end + 4.0 * FRAME) / FRAME).round() as i64;
    assert!(
        frame_brightness(&sidecar, &out, after) <= lit_threshold,
        "captions should be gone after the last cue ends (frame {after})"
    );
}

#[test]
fn every_background_and_caption_template_renders() {
    let Ok(sidecar) = FfmpegSidecar::locate() else {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    };

    // Cover art is synthesized by ffmpeg — no binary fixtures in the repo.
    let art = tmp_dir().join("clip_cover.png");
    let made = Command::new(sidecar.binary())
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=size=640x640:duration=1"])
        .args(["-frames:v", "1"])
        .arg(&art)
        .status();
    assert!(
        matches!(made, Ok(s) if s.success()),
        "build the cover fixture"
    );

    let audio = tone(10.0);
    let words = words();
    let cases = [
        (
            Background::solid("#101014"),
            CaptionStyle::Bold,
            Aspect::Square,
        ),
        (
            Background::waveform(),
            CaptionStyle::Minimal,
            Aspect::Vertical,
        ),
        (
            Background::cover_art(&art),
            CaptionStyle::Boxed,
            Aspect::Wide,
        ),
    ];

    for (i, (background, style, aspect)) in cases.into_iter().enumerate() {
        let spec = ClipSpec::new(5.0, 8.0)
            .with_aspect(aspect)
            .with_caption_style(style)
            .with_background(background)
            .with_title("Ep. 12 - the local bit");
        let out = tmp_dir().join(format!("clip_case_{i}.mp4"));
        render_clip(&audio, &words, &spec, &out).unwrap_or_else(|e| panic!("case {i}: {e}"));

        let banner = banner(&sidecar, &out);
        assert!(banner.contains("Video: h264"), "case {i}:\n{banner}");
        assert!(banner.contains("Audio: aac"), "case {i}:\n{banner}");
        let (w, h) = aspect.dimensions();
        assert!(banner.contains(&format!("{w}x{h}")), "case {i}:\n{banner}");
        assert!(
            (duration_secs(&banner) - 3.0).abs() < 0.15,
            "case {i} should be 3 s:\n{banner}"
        );
    }
}

#[test]
fn the_encode_route_is_lgpl_safe() {
    let Ok(sidecar) = FfmpegSidecar::locate() else {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    };
    let encoder = sidecar.h264_encoder().expect("an LGPL-safe H.264 encoder");
    assert!(
        anvil_media::LGPL_H264_ENCODERS.contains(&encoder.as_str()),
        "picked `{encoder}`, which is not on the LGPL-safe list"
    );
    assert!(
        !anvil_media::is_gpl_video_encoder(&encoder),
        "picked the GPL-linked `{encoder}` — handoff/07 §2 forbids it"
    );
    eprintln!("clip H.264 encoder selected: {encoder}");
}

#[test]
fn an_inverted_or_empty_range_is_refused_before_ffmpeg_runs() {
    let audio = tone(5.0);
    let out = tmp_dir().join("clip_never_written.mp4");
    let _ = std::fs::remove_file(&out);

    let err = render_clip(&audio, &[], &ClipSpec::new(9.0, 3.0), &out).unwrap_err();
    assert!(err.to_string().contains("invalid clip"), "{err}");

    // A range past the end of the buffer has no audio to render.
    let err = render_clip(&audio, &[], &ClipSpec::new(30.0, 40.0), &out);
    if FfmpegSidecar::locate().is_ok() {
        assert!(
            err.unwrap_err().to_string().contains("no audio"),
            "past the end"
        );
    }
    assert!(!out.exists(), "a rejected spec must not write a file");
}

/// 04 acceptance: **a 30 s clip renders in under 2 minutes on a 4-core CPU.**
#[test]
fn a_30s_clip_renders_inside_the_two_minute_budget() {
    let Ok(_sidecar) = FfmpegSidecar::locate() else {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    };

    // 30 s of audio, ~150 words — a realistic talking-head clip at 1080x1920.
    let audio = tone(35.0);
    let words: Vec<ClipWord> = (0..150)
        .map(|i| {
            let start = f64::from(i) * 0.2;
            ClipWord::new(format!("word{}", i % 20), start, start + 0.18)
        })
        .collect();
    let spec = ClipSpec::new(0.0, 30.0)
        .with_aspect(Aspect::Vertical)
        .with_caption_style(CaptionStyle::Bold)
        .with_background(Background::waveform())
        .with_title("Perf check");
    let out = tmp_dir().join("clip_perf_30s.mp4");

    let started = Instant::now();
    render_clip(&audio, &words, &spec, &out).expect("render the 30 s clip");
    let elapsed = started.elapsed();

    eprintln!(
        "30 s clip (1080x1920, waveform bg, 150 karaoke words) rendered in {:.1} s on {} logical cores",
        elapsed.as_secs_f64(),
        std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get)
    );
    assert!(
        elapsed.as_secs_f64() < 120.0,
        "04 acceptance: a 30 s clip must render in under 2 minutes, took {:.1} s",
        elapsed.as_secs_f64()
    );
}
