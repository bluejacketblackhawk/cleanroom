//! Encoder round-trip tests (M2): encode a synthesized tone to every target format, decode it
//! back through the normal decode path, and check the result is plausible (right rate, right
//! channel count, roughly the right duration, not silence).
//!
//! Every test here needs ffmpeg — there is no non-ffmpeg encode path in this crate (ADR-005:
//! encoders are ffmpeg-sidecar-only) — so they all gate on [`FfmpegSidecar::locate`] and skip
//! cleanly when it's absent, same pattern as the ffmpeg-gated tests in `tests/decode.rs`.

use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use anvil_media::{decode_to_buffer, AudioBuffer, Chapter, FfmpegSidecar, OutputSpec};

const FREQ: f64 = 440.0;
const DURATION_SECS: f64 = 1.0;
const SAMPLE_RATE: u32 = 48_000;

fn tmp_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn sine_buffer(channels: usize) -> AudioBuffer {
    let frames = (DURATION_SECS * SAMPLE_RATE as f64) as usize;
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for n in 0..frames {
        let t = n as f64 / SAMPLE_RATE as f64;
        let sample = ((2.0 * PI * FREQ * t).sin() * 0.5) as f32;
        for channel in planar.iter_mut() {
            channel.push(sample);
        }
    }
    AudioBuffer::from_planar(planar, SAMPLE_RATE)
}

fn rms(buffer: &AudioBuffer) -> f32 {
    let ch = buffer.channel(0);
    if ch.is_empty() {
        return 0.0;
    }
    let sum: f32 = ch.iter().map(|s| s * s).sum();
    (sum / ch.len() as f32).sqrt()
}

/// Decode `path` back and sanity-check it against the sine fixture. `duration_tolerance` is
/// generous (lossy codecs pad/trim frames at codec-specific block boundaries).
fn assert_round_trips(path: &Path, expected_channels: usize, duration_tolerance: f64) {
    let decoded =
        decode_to_buffer(path).unwrap_or_else(|e| panic!("decode {}: {e}", path.display()));
    assert_eq!(
        decoded.sample_rate(),
        48_000,
        "must normalize back to 48 kHz"
    );
    assert_eq!(decoded.channel_count(), expected_channels);

    let expected = DURATION_SECS * 48_000.0;
    let actual = decoded.frames() as f64;
    let err = (actual - expected).abs() / expected;
    assert!(
        err < duration_tolerance,
        "{}: duration within {:.0}%, expected ~{expected} frames, got {actual}",
        path.display(),
        duration_tolerance * 100.0
    );
    assert!(
        rms(&decoded) > 0.05,
        "{}: round-tripped audio should not be silent",
        path.display()
    );
}

macro_rules! ffmpeg_gated_test {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            if FfmpegSidecar::locate().is_err() {
                eprintln!("skipping {}: ffmpeg unavailable", stringify!($name));
                return;
            }
            $body
        }
    };
}

ffmpeg_gated_test!(encode_mp3_round_trips, {
    let path = tmp_dir().join("enc_sine.mp3");
    anvil_media::encode(&sine_buffer(2), &OutputSpec::mp3(192), &path).expect("encode mp3");
    assert_round_trips(&path, 2, 0.1);
});

ffmpeg_gated_test!(encode_opus_round_trips, {
    let path = tmp_dir().join("enc_sine.opus");
    anvil_media::encode(&sine_buffer(2), &OutputSpec::opus(96), &path).expect("encode opus");
    // Opus's own internal delay/pre-skip padding is a bigger fraction of a 1s clip than the
    // other codecs' block padding, so give it a wider tolerance.
    assert_round_trips(&path, 2, 0.2);
});

ffmpeg_gated_test!(encode_vorbis_round_trips, {
    let path = tmp_dir().join("enc_sine.ogg");
    anvil_media::encode(&sine_buffer(2), &OutputSpec::vorbis(160), &path).expect("encode vorbis");
    assert_round_trips(&path, 2, 0.1);
});

ffmpeg_gated_test!(encode_flac_16bit_round_trips, {
    let path = tmp_dir().join("enc_sine_16.flac");
    anvil_media::encode(&sine_buffer(2), &OutputSpec::flac(16), &path).expect("encode flac16");
    assert_round_trips(&path, 2, 0.05);
});

ffmpeg_gated_test!(encode_flac_24bit_round_trips, {
    let path = tmp_dir().join("enc_sine_24.flac");
    anvil_media::encode(&sine_buffer(2), &OutputSpec::flac(24), &path).expect("encode flac24");
    assert_round_trips(&path, 2, 0.05);
});

ffmpeg_gated_test!(encode_aac_round_trips, {
    let path = tmp_dir().join("enc_sine.m4a");
    anvil_media::encode(&sine_buffer(2), &OutputSpec::aac(160), &path).expect("encode aac");
    assert_round_trips(&path, 2, 0.1);
});

ffmpeg_gated_test!(encode_m4b_round_trips, {
    let path = tmp_dir().join("enc_sine.m4b");
    anvil_media::encode(&sine_buffer(1), &OutputSpec::m4b(128), &path).expect("encode m4b");
    assert_round_trips(&path, 1, 0.1);
});

ffmpeg_gated_test!(mono_downmix_produces_one_channel, {
    let path = tmp_dir().join("enc_sine_mono.mp3");
    let spec = OutputSpec::mp3(128).with_mono(true);
    anvil_media::encode(&sine_buffer(2), &spec, &path).expect("encode mono mp3");
    assert_round_trips(&path, 1, 0.1);
});

ffmpeg_gated_test!(encode_multi_writes_every_target_from_one_buffer, {
    let buffer = sine_buffer(2);
    let mp3_path = tmp_dir().join("multi.mp3");
    let flac_path = tmp_dir().join("multi.flac");
    let ogg_path = tmp_dir().join("multi.ogg");

    anvil_media::encode_multi(
        &buffer,
        &[
            (OutputSpec::mp3(192), mp3_path.clone()),
            (OutputSpec::flac(16), flac_path.clone()),
            (OutputSpec::vorbis(160), ogg_path.clone()),
        ],
    )
    .expect("encode_multi");

    assert_round_trips(&mp3_path, 2, 0.1);
    assert_round_trips(&flac_path, 2, 0.05);
    assert_round_trips(&ogg_path, 2, 0.1);
});

ffmpeg_gated_test!(m4b_audiobook_embeds_chapters_in_one_pass, {
    let path = tmp_dir().join("audiobook.m4b");
    let chapters = vec![
        Chapter {
            title: "Chapter One".into(),
            start_ms: 0,
            end_ms: 500,
        },
        Chapter {
            title: "Chapter Two".into(),
            start_ms: 500,
            end_ms: 1000,
        },
    ];
    anvil_media::encode_m4b_audiobook(
        &sine_buffer(1),
        &OutputSpec::m4b(96),
        &chapters,
        &path,
        |_| {},
    )
    .expect("encode m4b audiobook");
    assert_round_trips(&path, 1, 0.1);

    let sidecar = FfmpegSidecar::locate().unwrap();
    let read_back = anvil_media::read_chapters(&sidecar, &path).expect("read chapters back");
    assert_eq!(read_back, chapters);
});

ffmpeg_gated_test!(m4b_audiobook_without_chapters_still_encodes, {
    let path = tmp_dir().join("audiobook_no_chapters.m4b");
    anvil_media::encode_m4b_audiobook(&sine_buffer(1), &OutputSpec::m4b(96), &[], &path, |_| {})
        .expect("encode m4b audiobook without chapters");
    assert_round_trips(&path, 1, 0.1);
});
