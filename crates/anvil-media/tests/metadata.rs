//! Metadata/chapters tests (M2).
//!
//! Tag round-trip tests use a FLAC fixture written by `flacenc` (pure Rust, a dev-dep
//! already used in `tests/decode.rs`) — no ffmpeg needed, so these **always run**, proving
//! the round-trip rule ("never drop tags the user didn't touch") independent of the sidecar.
//! Chapter tests need ffmpeg (lofty has no chapter support at all — see `src/metadata.rs`)
//! and skip cleanly when it's absent.

use std::path::{Path, PathBuf};
use std::process::Command;

use anvil_media::{Chapter, CoverArt, FfmpegSidecar, TagEditor};

fn tmp_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// A tiny FLAC fixture (silence is fine — these tests only care about tags, not audio
/// content). Mirrors `tests/decode.rs`'s `write_flac` helper.
fn write_flac(path: &Path) {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let sample_rate = 48_000usize;
    let frames = sample_rate / 10; // 100 ms
    let interleaved: Vec<i32> = (0..frames).map(|n| ((n % 100) as i32 - 50) * 100).collect();

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| e)
        .expect("default flac config verifies");
    let source = flacenc::source::MemSource::from_samples(&interleaved, 1, 16, sample_rate);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .expect("flac encode");
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).expect("flac serialize");
    std::fs::write(path, sink.as_slice()).expect("write flac");
}

// ---- lofty tag round-trip: always runs -----------------------------------------------------

#[test]
fn round_trip_preserves_untouched_fields() {
    let path = tmp_dir().join("tags_roundtrip.flac");
    write_flac(&path);

    // Seed several fields plus cover art in one pass.
    {
        let mut editor = TagEditor::open(&path).expect("open for seeding");
        editor.set_title("Episode 1");
        editor.set_artist("Rob");
        editor.set_album("The Show");
        editor.set_genre("Podcast");
        editor.set_comment("first cut");
        editor.set_track(1);
        editor.set_date("2026-07-13").expect("valid date");
        editor.set_cover_art(CoverArt {
            mime: "image/jpeg".to_string(),
            data: vec![0xFF, 0xD8, 0xFF, 0xDB, 1, 2, 3, 4],
        });
        editor.save().expect("save seeded tags");
    }

    // Touch ONLY the title on the next open.
    {
        let mut editor = TagEditor::open(&path).expect("reopen");
        assert_eq!(editor.title().as_deref(), Some("Episode 1"));
        editor.set_title("Episode 1 (Remastered)");
        editor.save().expect("save title-only change");
    }

    // Everything untouched must still be there; only the title changed. This is the
    // round-trip proof the M2 deliverable requires.
    let editor = TagEditor::open(&path).expect("final open");
    assert_eq!(editor.title().as_deref(), Some("Episode 1 (Remastered)"));
    assert_eq!(editor.artist().as_deref(), Some("Rob"));
    assert_eq!(editor.album().as_deref(), Some("The Show"));
    assert_eq!(editor.genre().as_deref(), Some("Podcast"));
    assert_eq!(editor.comment().as_deref(), Some("first cut"));
    assert_eq!(editor.track(), Some(1));
    assert_eq!(editor.date().as_deref(), Some("2026-07-13"));

    let cover = editor.cover_art().expect("cover art survived");
    assert_eq!(cover.mime, "image/jpeg");
    assert_eq!(cover.data, vec![0xFF, 0xD8, 0xFF, 0xDB, 1, 2, 3, 4]);
}

#[test]
fn opening_and_saving_without_edits_creates_no_tag() {
    let path = tmp_dir().join("tags_untouched.flac");
    write_flac(&path);

    {
        let mut editor = TagEditor::open(&path).expect("open");
        editor.save().expect("save without touching anything");
    }

    let editor = TagEditor::open(&path).expect("reopen");
    assert_eq!(editor.title(), None);
    assert_eq!(editor.artist(), None);
    assert_eq!(editor.cover_art(), None);
}

#[test]
fn cover_art_can_be_replaced_and_removed() {
    let path = tmp_dir().join("tags_cover.flac");
    write_flac(&path);

    let mut editor = TagEditor::open(&path).expect("open");
    editor.set_cover_art(CoverArt {
        mime: "image/png".to_string(),
        data: vec![1, 2, 3],
    });
    editor.save().expect("save cover");

    let mut editor = TagEditor::open(&path).expect("reopen");
    assert_eq!(editor.cover_art().unwrap().data, vec![1, 2, 3]);

    editor.remove_cover_art();
    editor.save().expect("save removal");

    let editor = TagEditor::open(&path).expect("reopen after removal");
    assert_eq!(editor.cover_art(), None);
}

#[test]
fn save_id3v23_does_not_error_on_non_id3_container() {
    // FLAC's primary tag type is Vorbis comments, not ID3v2 — `save_id3v23` should just be a
    // no-op version choice for that tag type, not an error.
    let path = tmp_dir().join("tags_id3v23_noop.flac");
    write_flac(&path);
    let mut editor = TagEditor::open(&path).expect("open");
    editor.set_title("x");
    editor
        .save_id3v23()
        .expect("save_id3v23 on a non-ID3 container should not fail");
}

// ---- chapters: ffmpeg-gated, skip cleanly when absent ---------------------------------------

fn ffmpeg_sidecar() -> Option<FfmpegSidecar> {
    FfmpegSidecar::locate().ok()
}

/// Build an mp3 fixture with title/artist tags via ffmpeg directly (independent of our own
/// encode path, so this test isolates metadata behavior).
fn make_tagged_mp3(sidecar: &FfmpegSidecar, path: &Path) -> bool {
    let status = Command::new(sidecar.binary())
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
        .args(["-c:a", "libmp3lame", "-b:a", "128k"])
        .args(["-metadata", "title=My Episode", "-metadata", "artist=Rob"])
        .arg(path)
        .status();
    matches!(status, Ok(s) if s.success()) && path.is_file()
}

#[test]
fn write_chapters_then_read_back_preserves_tags() {
    let Some(sidecar) = ffmpeg_sidecar() else {
        eprintln!("skipping write_chapters_then_read_back_preserves_tags: ffmpeg unavailable");
        return;
    };
    let path = tmp_dir().join("chapters_mp3.mp3");
    if !make_tagged_mp3(&sidecar, &path) {
        eprintln!(
            "skipping write_chapters_then_read_back_preserves_tags: could not build mp3 fixture"
        );
        return;
    }

    let chapters = vec![
        Chapter {
            title: "Intro".into(),
            start_ms: 0,
            end_ms: 1000,
        },
        Chapter {
            title: "Body".into(),
            start_ms: 1000,
            end_ms: 2000,
        },
    ];
    anvil_media::write_chapters(&sidecar, &path, &chapters).expect("write chapters");

    let read_back = anvil_media::read_chapters(&sidecar, &path).expect("read chapters");
    assert_eq!(read_back, chapters);

    // The whole point of routing chapters through -map_metadata 0: the pre-existing ID3
    // title/artist must survive the chapter-embedding remux untouched.
    let info_editor = TagEditor::open(&path).expect("open after chapter write");
    assert_eq!(info_editor.title().as_deref(), Some("My Episode"));
    assert_eq!(info_editor.artist().as_deref(), Some("Rob"));
}

#[test]
fn no_chapters_reads_back_empty() {
    let Some(sidecar) = ffmpeg_sidecar() else {
        eprintln!("skipping no_chapters_reads_back_empty: ffmpeg unavailable");
        return;
    };
    let path = tmp_dir().join("no_chapters.mp3");
    if !make_tagged_mp3(&sidecar, &path) {
        eprintln!("skipping no_chapters_reads_back_empty: could not build mp3 fixture");
        return;
    }
    let chapters = anvil_media::read_chapters(&sidecar, &path).expect("read chapters");
    assert!(chapters.is_empty());
}
