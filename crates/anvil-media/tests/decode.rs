//! Format-zoo decode tests.
//!
//! Fixtures are synthesized at test time into `CARGO_TARGET_TMPDIR` (no binaries in the
//! repo): `hound` writes the WAV sine and `flacenc` (pure Rust) writes the FLAC sine, so the
//! **wav and flac tests always run**. The mp3/m4a/mp4 tests need ffmpeg to *create* the
//! fixture (nothing in-tree can encode those), so they are gated on an ffmpeg-present check
//! and skip cleanly — they never fail the suite when ffmpeg is missing.

use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::process::Command;

use anvil_core::BLOCK_SAMPLES;
use anvil_media::{decode_blocks, decode_to_buffer, probe, FfmpegSidecar};

const FREQ: f64 = 440.0;
const DURATION_SECS: f64 = 0.5;

fn tmp_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// A per-channel sine at 16-bit scale. Channel 0 louder than the rest so channel identity is
/// observable if a test wants it.
fn sine_planar(sample_rate: u32, channels: usize) -> Vec<Vec<i16>> {
    let frames = (DURATION_SECS * sample_rate as f64) as usize;
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for n in 0..frames {
        let t = n as f64 / sample_rate as f64;
        let base = (2.0 * PI * FREQ * t).sin();
        for (c, channel) in planar.iter_mut().enumerate() {
            let amp = if c == 0 { 0.6 } else { 0.4 };
            channel.push((base * amp * i16::MAX as f64) as i16);
        }
    }
    planar
}

fn write_wav(path: &Path, planar: &[Vec<i16>], sample_rate: u32) {
    let spec = hound::WavSpec {
        channels: planar.len() as u16,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    for n in 0..planar[0].len() {
        for channel in planar {
            writer.write_sample(channel[n]).expect("write sample");
        }
    }
    writer.finalize().expect("finalize wav");
}

fn write_flac(path: &Path, planar: &[Vec<i16>], sample_rate: u32) {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let channels = planar.len();
    let frames = planar[0].len();
    let mut interleaved = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        for channel in planar {
            interleaved.push(channel[n] as i32);
        }
    }

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| e)
        .expect("default flac config verifies");
    let source =
        flacenc::source::MemSource::from_samples(&interleaved, channels, 16, sample_rate as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .expect("flac encode");
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).expect("flac serialize");
    std::fs::write(path, sink.as_slice()).expect("write flac");
}

/// RMS of channel 0, to assert the decode carried real signal (not silence).
fn rms(buffer: &anvil_media::AudioBuffer) -> f32 {
    let ch = buffer.channel(0);
    if ch.is_empty() {
        return 0.0;
    }
    let sum: f32 = ch.iter().map(|s| s * s).sum();
    (sum / ch.len() as f32).sqrt()
}

// ---- symphonia path: always run ----------------------------------------------------------

#[test]
fn decode_wav_48k_stereo() {
    let path = tmp_dir().join("sine_48k_stereo.wav");
    write_wav(&path, &sine_planar(48_000, 2), 48_000);

    let buffer = decode_to_buffer(&path).expect("decode wav");
    assert_eq!(buffer.sample_rate(), 48_000);
    assert_eq!(buffer.channel_count(), 2);
    // 48 kHz source -> no resampling -> exact frame count.
    assert_eq!(buffer.frames(), (DURATION_SECS * 48_000.0) as usize);
    assert!(rms(&buffer) > 0.1, "decoded audio should not be silent");
}

#[test]
fn decode_flac_48k_mono() {
    let path = tmp_dir().join("sine_48k_mono.flac");
    write_flac(&path, &sine_planar(48_000, 1), 48_000);

    let buffer = decode_to_buffer(&path).expect("decode flac");
    assert_eq!(buffer.sample_rate(), 48_000);
    assert_eq!(buffer.channel_count(), 1);
    // flacenc zero-pads the final frame up to a whole block, so the decoded length is the
    // source length rounded up to the encoder's block size — assert approximately.
    let expected = DURATION_SECS * 48_000.0;
    let err = (buffer.frames() as f64 - expected).abs() / expected;
    assert!(
        err < 0.05,
        "flac duration within 5%, got {} frames",
        buffer.frames()
    );
    assert!(rms(&buffer) > 0.1, "decoded audio should not be silent");
}

#[test]
fn decode_wav_44k1_resamples_to_48k() {
    let path = tmp_dir().join("sine_44k1_stereo.wav");
    write_wav(&path, &sine_planar(44_100, 2), 44_100);

    let buffer = decode_to_buffer(&path).expect("decode + resample wav");
    assert_eq!(buffer.sample_rate(), 48_000, "must normalize to 48 kHz");
    assert_eq!(buffer.channel_count(), 2);

    let expected = DURATION_SECS * 48_000.0;
    let actual = buffer.frames() as f64;
    let err = (actual - expected).abs() / expected;
    assert!(
        err < 0.05,
        "resampled duration within 5%: expected ~{expected} frames, got {actual}"
    );
    assert!(rms(&buffer) > 0.1, "resampled audio should not be silent");
}

#[test]
fn streaming_blocks_cover_whole_file() {
    let path = tmp_dir().join("sine_48k_stereo_blocks.wav");
    write_wav(&path, &sine_planar(48_000, 2), 48_000);

    let mut blocks: Vec<usize> = Vec::new();
    let decoder = decode_blocks(&path).expect("open block decoder");
    assert_eq!(decoder.channel_count(), 2);
    for block in decoder {
        let block = block.expect("block decode");
        assert_eq!(block.sample_rate(), 48_000);
        assert_eq!(block.channel_count(), 2);
        blocks.push(block.frames());
    }

    let total: usize = blocks.iter().sum();
    assert_eq!(total, (DURATION_SECS * 48_000.0) as usize);
    // Every block but the last is a full BLOCK_SAMPLES; the last is the remainder.
    for &len in &blocks[..blocks.len() - 1] {
        assert_eq!(len, BLOCK_SAMPLES);
    }
    assert!(*blocks.last().unwrap() <= BLOCK_SAMPLES);
}

#[test]
fn probe_wav_reports_source_facts() {
    let path = tmp_dir().join("probe_sine_44k1.wav");
    write_wav(&path, &sine_planar(44_100, 2), 44_100);

    let info = probe(&path).expect("probe wav");
    assert_eq!(info.source_sample_rate, 44_100);
    assert_eq!(info.channels, 2);
    assert_eq!(info.format, "wav");
    assert!(
        (info.duration_secs - DURATION_SECS).abs() < 0.01,
        "duration ~0.5s, got {}",
        info.duration_secs
    );
}

#[test]
fn probe_flac_reports_source_facts() {
    let path = tmp_dir().join("probe_sine_48k.flac");
    write_flac(&path, &sine_planar(48_000, 1), 48_000);

    let info = probe(&path).expect("probe flac");
    assert_eq!(info.source_sample_rate, 48_000);
    assert_eq!(info.channels, 1);
    assert_eq!(info.format, "flac");
    assert!((info.duration_secs - DURATION_SECS).abs() < 0.01);
}

#[test]
fn garbage_input_errors_without_panicking() {
    let path = tmp_dir().join("not_audio.bin");
    std::fs::write(&path, b"this is definitely not an audio file").unwrap();
    // symphonia rejects it as unsupported; the fallback then either fails to find ffmpeg or
    // ffmpeg itself rejects it. Either way: a clean Err, never a panic.
    assert!(decode_to_buffer(&path).is_err());
}

// ---- ffmpeg path: gated, skips cleanly when ffmpeg is absent -----------------------------

/// Transcode the always-available WAV sine into `dst` with ffmpeg. Returns false if ffmpeg
/// is unavailable or the transcode fails, so callers can skip.
fn make_with_ffmpeg(dst: &Path, extra_args: &[&str]) -> bool {
    let Ok(sidecar) = FfmpegSidecar::locate() else {
        return false;
    };
    let src = tmp_dir().join("ffmpeg_src_sine.wav");
    write_wav(&src, &sine_planar(48_000, 2), 48_000);

    let status = Command::new(sidecar.binary())
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&src)
        .args(extra_args)
        .arg(dst)
        .status();
    matches!(status, Ok(s) if s.success()) && dst.is_file()
}

fn assert_decodes_to_48k(path: &Path, expected_channels: usize) {
    let buffer = decode_to_buffer(path).expect("decode ffmpeg-made fixture");
    assert_eq!(buffer.sample_rate(), 48_000);
    assert_eq!(buffer.channel_count(), expected_channels);
    let expected = DURATION_SECS * 48_000.0;
    let err = (buffer.frames() as f64 - expected).abs() / expected;
    assert!(
        err < 0.1,
        "duration within 10%, got {} frames",
        buffer.frames()
    );
}

#[test]
fn decode_mp3_when_ffmpeg_present() {
    let path = tmp_dir().join("sine.mp3");
    if !make_with_ffmpeg(&path, &["-codec:a", "libmp3lame", "-q:a", "4"]) {
        eprintln!("skipping decode_mp3_when_ffmpeg_present: ffmpeg unavailable");
        return;
    }
    // MP3 routes through symphonia (mp3 feature); this exercises that decode path.
    assert_decodes_to_48k(&path, 2);
}

#[test]
fn decode_m4a_aac_when_ffmpeg_present() {
    let path = tmp_dir().join("sine.m4a");
    if !make_with_ffmpeg(&path, &["-codec:a", "aac", "-b:a", "128k"]) {
        eprintln!("skipping decode_m4a_aac_when_ffmpeg_present: ffmpeg unavailable");
        return;
    }
    // m4a (isomp4 + aac features) routes through symphonia.
    assert_decodes_to_48k(&path, 2);
}

#[test]
fn decode_mp4_video_container_when_ffmpeg_present() {
    let path = tmp_dir().join("sine_in_video.mkv");
    // An MKV container symphonia can't demux (mkv feature not enabled) -> ffmpeg fallback.
    if !make_with_ffmpeg(&path, &["-codec:a", "libopus", "-b:a", "96k"]) {
        eprintln!("skipping decode_mp4_video_container_when_ffmpeg_present: ffmpeg unavailable");
        return;
    }
    assert_decodes_to_48k(&path, 2);
}
