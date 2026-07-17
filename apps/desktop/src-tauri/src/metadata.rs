//! Chapters & Metadata tab (04 §S2): read the open file's standard tags + chapters, edit
//! title/artist/album/genre/date/comment/track/cover-art and add/rename/reorder chapters,
//! and write them back to a target file.
//!
//! This module is a thin Tauri surface over `anvil_media`'s [`TagEditor`] (standard tags +
//! cover art, round-tripped through lofty) and [`read_chapters`]/[`write_chapters`] (ID3v2
//! CHAP / MP4 chapter atoms via the ffmpeg `ffmetadata` path, since lofty has no chapter
//! support — see that crate's module docs). The engine does the hard part; the app only maps
//! the wire shape onto it and reports honest errors.
//!
//! ## Where edits are written
//! [`metadata_read`] reads from the currently open file (`AudioState::source_path`).
//! [`metadata_write`] writes to an explicit `target` path — the UI defaults it to the source
//! file (edit in place) but the field is editable, so a user can also tag a mastered export
//! (04 §S2 "applies to all exports"). Tags are written first with lofty; chapters are then
//! remuxed on with ffmpeg's `-map_metadata 0 -map_chapters 1 -c copy`, which takes the tags
//! from the just-edited file and only the chapters from a synthetic ffmetadata file, so the
//! two steps never clobber each other (see [`write_chapters`]).
//!
//! ## Missing sidecar
//! Standard tags need no sidecar (lofty is linked). Chapters need the ffmpeg sidecar: reading
//! them is best-effort — if ffmpeg can't be located/verified the tags still come back, just
//! with an empty chapter list and a note — and writing a non-empty chapter list returns a
//! clean, actionable error when the sidecar is absent rather than crashing.

use std::path::{Path, PathBuf};

use anvil_media::{read_chapters, write_chapters, Chapter, CoverArt, FfmpegSidecar, TagEditor};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::AudioState;

/// One chapter marker on the wire (04 §S2 "chapter list — time, title"). `start_ms`/`end_ms`
/// are millisecond offsets from the start of the file, matching [`anvil_media::Chapter`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChapterWire {
    pub title: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

impl From<&Chapter> for ChapterWire {
    fn from(c: &Chapter) -> Self {
        ChapterWire {
            title: c.title.clone(),
            start_ms: c.start_ms,
            end_ms: c.end_ms,
        }
    }
}

impl From<&ChapterWire> for Chapter {
    fn from(c: &ChapterWire) -> Self {
        Chapter {
            title: c.title.clone(),
            start_ms: c.start_ms,
            end_ms: c.end_ms,
        }
    }
}

/// The Chapters & Metadata tab's read model: the file's standard tags, its cover art (as a
/// `data:` URL the UI renders directly), and its chapters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileMetadata {
    /// The file these tags were read from (the currently open source file).
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    /// Release date/timestamp string (`"2026-07-13"`, a year, or a full ISO timestamp).
    pub date: Option<String>,
    pub comment: Option<String>,
    /// Track (episode) number.
    pub track: Option<u32>,
    /// Existing cover art as a `data:<mime>;base64,<…>` URL, or `None` if the file has none.
    pub cover_art: Option<String>,
    /// The cover art's MIME type (`image/jpeg`, …), when present.
    pub cover_mime: Option<String>,
    pub chapters: Vec<ChapterWire>,
    /// `false` when chapters couldn't be read because the ffmpeg sidecar was unavailable —
    /// the tags above are still valid, chapters just couldn't be inspected. See `chapters_note`.
    pub chapters_available: bool,
    /// Why chapters couldn't be read, if `chapters_available` is `false`.
    pub chapters_note: Option<String>,
}

/// The Chapters & Metadata tab's write model: the edited tags + chapters to apply to a file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetadataEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub comment: Option<String>,
    pub track: Option<u32>,
    /// Path to a new cover-art image to embed (04 §S2 "cover art drag-drop"). `None` leaves
    /// any existing cover art untouched.
    pub cover_art_path: Option<String>,
    /// Remove existing cover art (wins over `cover_art_path` being set).
    #[serde(default)]
    pub remove_cover_art: bool,
    /// The complete chapter list to write. Empty means "no chapters" — see [`metadata_write`].
    #[serde(default)]
    pub chapters: Vec<ChapterWire>,
}

/// Read the currently open file's tags + chapters (04 §S2 Chapters & Metadata tab).
///
/// Standard tags and cover art come from lofty (no sidecar needed). Chapters are read
/// best-effort from the ffmpeg banner: if the sidecar is missing/unverified the tags still
/// return, with an empty chapter list and `chapters_available = false`.
#[tauri::command]
pub fn metadata_read(state: State<'_, AudioState>) -> Result<FileMetadata, String> {
    let path = {
        let guard = state.source_path.read().map_err(|_| "path lock poisoned")?;
        guard
            .as_ref()
            .ok_or_else(|| "open a file before reading its metadata".to_string())?
            .clone()
    };
    read_metadata_from(&path)
}

/// Read `path`'s tags + chapters into a [`FileMetadata`]. The command's real work, split out
/// so it's testable against a real file without a running Tauri app.
fn read_metadata_from(path: &Path) -> Result<FileMetadata, String> {
    let editor = TagEditor::open(path)
        .map_err(|e| format!("could not read tags from {}: {e}", path.display()))?;

    let cover = editor.cover_art();
    let (cover_art, cover_mime) = match cover {
        Some(c) => (Some(cover_data_url(&c)), Some(c.mime)),
        None => (None, None),
    };

    let (chapters, chapters_available, chapters_note) = read_chapters_best_effort(path);

    Ok(FileMetadata {
        path: path.to_string_lossy().into_owned(),
        title: editor.title(),
        artist: editor.artist(),
        album: editor.album(),
        genre: editor.genre(),
        date: editor.date(),
        comment: editor.comment(),
        track: editor.track(),
        cover_art,
        cover_mime,
        chapters,
        chapters_available,
        chapters_note,
    })
}

/// Read chapters from `path`, degrading to an empty list + a note when the ffmpeg sidecar
/// (which chapter reading needs — see the module docs) can't be located or verified.
fn read_chapters_best_effort(path: &Path) -> (Vec<ChapterWire>, bool, Option<String>) {
    let sidecar = match FfmpegSidecar::locate() {
        Ok(s) => s,
        Err(e) => {
            return (
                Vec::new(),
                false,
                Some(format!(
                    "chapters need the ffmpeg component, which isn't available: {e}"
                )),
            );
        }
    };
    match read_chapters(&sidecar, path) {
        Ok(chapters) => (chapters.iter().map(ChapterWire::from).collect(), true, None),
        Err(e) => (
            Vec::new(),
            false,
            Some(format!("could not read chapters: {e}")),
        ),
    }
}

/// Apply the edited tags + chapters to `target` (04 §S2 "applies to all exports").
///
/// Tags/cover art are written first with lofty (in place); then, when `edit.chapters` is
/// non-empty, the chapters are remuxed on with the ffmpeg sidecar (`-map_metadata 0`, so the
/// tags just written are preserved). An empty chapter list writes no chapters and needs no
/// sidecar — so a tags-only edit works even when ffmpeg isn't present. Writing a non-empty
/// chapter list without the sidecar is a clean, actionable error, never a crash.
#[tauri::command]
pub fn metadata_write(target: String, edit: MetadataEdit) -> Result<(), String> {
    write_metadata_to(&PathBuf::from(&target), &edit)
}

/// Apply `edit`'s tags + chapters to `path`. The command's real work, split out so it's
/// testable against a real file without a running Tauri app.
fn write_metadata_to(path: &Path, edit: &MetadataEdit) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("no file to tag at {}", path.display()));
    }

    // --- standard tags + cover art (lofty) ---
    let mut editor = TagEditor::open(path)
        .map_err(|e| format!("could not open {} for tagging: {e}", path.display()))?;

    if let Some(v) = &edit.title {
        editor.set_title(v.clone());
    }
    if let Some(v) = &edit.artist {
        editor.set_artist(v.clone());
    }
    if let Some(v) = &edit.album {
        editor.set_album(v.clone());
    }
    if let Some(v) = &edit.genre {
        editor.set_genre(v.clone());
    }
    if let Some(v) = &edit.comment {
        editor.set_comment(v.clone());
    }
    if let Some(v) = edit.track {
        editor.set_track(v);
    }
    if let Some(v) = &edit.date {
        if !v.trim().is_empty() {
            editor
                .set_date(v.trim())
                .map_err(|e| format!("{e} — use a year or a YYYY-MM-DD date"))?;
        }
    }

    if edit.remove_cover_art {
        editor.remove_cover_art();
    } else if let Some(cover_path) = edit.cover_art_path.as_deref().map(str::trim) {
        if !cover_path.is_empty() {
            let cover = read_cover_art(Path::new(cover_path))?;
            editor.set_cover_art(cover);
        }
    }

    editor
        .save()
        .map_err(|e| format!("could not save tags to {}: {e}", path.display()))?;

    // --- chapters (ffmpeg ffmetadata) ---
    if !edit.chapters.is_empty() {
        let sidecar = FfmpegSidecar::locate().map_err(|e| {
            format!("chapters need the ffmpeg component, which isn't available: {e}")
        })?;
        let chapters: Vec<Chapter> = edit.chapters.iter().map(Chapter::from).collect();
        write_chapters(&sidecar, path, &chapters)
            .map_err(|e| format!("could not write chapters to {}: {e}", path.display()))?;
    }

    Ok(())
}

/// Read a cover-art image file into a [`CoverArt`], inferring the MIME type from the file
/// extension (the podcast/audiobook cover formats: jpeg/png, plus the other lofty knows).
fn read_cover_art(path: &Path) -> Result<CoverArt, String> {
    if !path.is_file() {
        return Err(format!("no cover image at {}", path.display()));
    }
    let data =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mime = mime_from_extension(path);
    Ok(CoverArt { mime, data })
}

/// MIME type for a cover-art file, from its extension. Defaults to `image/jpeg` (the podcast
/// cover default) for an unrecognised/absent extension rather than failing — the bytes are
/// what matter, and every player sniffs the actual format anyway.
fn mime_from_extension(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("tif") | Some("tiff") => "image/tiff",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
    .to_string()
}

/// Build a `data:<mime>;base64,<…>` URL from cover-art bytes so the UI can render the existing
/// artwork in an `<img>` without a second round-trip or an asset-protocol handler.
fn cover_data_url(cover: &CoverArt) -> String {
    format!("data:{};base64,{}", cover.mime, base64_encode(&cover.data))
}

/// Minimal standard-alphabet base64 encoder (RFC 4648), std-only so a cover-art data URL
/// needs no new dependency. Cover art is a few hundred KB at most.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_wire_round_trips_through_the_engine_type() {
        let wire = ChapterWire {
            title: "Intro".into(),
            start_ms: 0,
            end_ms: 1000,
        };
        let engine = Chapter::from(&wire);
        assert_eq!(engine.title, "Intro");
        assert_eq!(engine.start_ms, 0);
        assert_eq!(engine.end_ms, 1000);
        let back = ChapterWire::from(&engine);
        assert_eq!(back, wire);
    }

    #[test]
    fn mime_from_extension_maps_the_common_cover_formats() {
        assert_eq!(mime_from_extension(Path::new("cover.png")), "image/png");
        assert_eq!(mime_from_extension(Path::new("cover.JPG")), "image/jpeg");
        assert_eq!(mime_from_extension(Path::new("cover.jpeg")), "image/jpeg");
        assert_eq!(mime_from_extension(Path::new("cover.gif")), "image/gif");
        // Unknown/absent extension defaults to jpeg rather than failing.
        assert_eq!(mime_from_extension(Path::new("cover.xyz")), "image/jpeg");
        assert_eq!(mime_from_extension(Path::new("cover")), "image/jpeg");
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 §10 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn cover_data_url_has_the_mime_and_base64_payload() {
        let cover = CoverArt {
            mime: "image/png".into(),
            data: b"foobar".to_vec(),
        };
        assert_eq!(cover_data_url(&cover), "data:image/png;base64,Zm9vYmFy");
    }

    #[test]
    fn read_cover_art_reads_bytes_and_infers_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.png");
        std::fs::write(&path, b"\x89PNG fake bytes").unwrap();
        let cover = read_cover_art(&path).expect("read cover");
        assert_eq!(cover.mime, "image/png");
        assert_eq!(cover.data, b"\x89PNG fake bytes");
    }

    #[test]
    fn read_cover_art_missing_file_is_a_clean_error() {
        let err = read_cover_art(Path::new("Z:/does/not/exist.png")).unwrap_err();
        assert!(err.contains("cover image"));
    }

    /// End-to-end against a real audio file, through the exact helpers the Tauri commands call
    /// (`write_metadata_to` / `read_metadata_from`). Gated on `CLEANROOM_FFMPEG` (used to *create*
    /// the fixture); the chapter round-trip additionally needs a usable ffmpeg sidecar
    /// (`FfmpegSidecar::locate`), so it's asserted only when one is available (set
    /// `CLEANROOM_FFMPEG_ALLOW_UNPINNED=1` for a dev/GPL ffmpeg) — otherwise the tag round-trip is
    /// still verified and chapters are confirmed to degrade cleanly (empty + a note).
    #[test]
    fn metadata_round_trips_tags_and_chapters_on_a_real_file() {
        let Some(ffmpeg) = std::env::var_os("CLEANROOM_FFMPEG") else {
            eprintln!("skipping: CLEANROOM_FFMPEG not set");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("clip.m4a");
        // A 2 s AAC/M4A tone via ffmpeg's built-in aac encoder (no external lib needed).
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "aac",
            ])
            .arg(&media)
            .status()
            .expect("spawn ffmpeg to build the fixture");
        assert!(status.success(), "ffmpeg could not build the m4a fixture");

        // --- tag round-trip (lofty, no sidecar needed) ---
        let tags_only = MetadataEdit {
            title: Some("Episode 12".into()),
            artist: Some("The Hosts".into()),
            album: Some("A Show".into()),
            comment: Some("wired end-to-end".into()),
            chapters: Vec::new(),
            ..Default::default()
        };
        write_metadata_to(&media, &tags_only).expect("write tags");
        let read = read_metadata_from(&media).expect("read back");
        assert_eq!(read.title.as_deref(), Some("Episode 12"));
        assert_eq!(read.artist.as_deref(), Some("The Hosts"));
        assert_eq!(read.album.as_deref(), Some("A Show"));

        // --- chapter round-trip (needs a usable ffmpeg sidecar) ---
        if FfmpegSidecar::locate().is_err() {
            let read = read_metadata_from(&media).expect("read back");
            assert!(
                !read.chapters_available && read.chapters_note.is_some(),
                "with no usable sidecar, chapters must degrade cleanly, not crash"
            );
            eprintln!("note: chapter round-trip skipped (no pinned/allowed ffmpeg sidecar)");
            return;
        }
        let with_chapters = MetadataEdit {
            title: Some("Episode 12".into()),
            chapters: vec![
                ChapterWire {
                    title: "Intro".into(),
                    start_ms: 0,
                    end_ms: 1000,
                },
                ChapterWire {
                    title: "Main".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                },
            ],
            ..Default::default()
        };
        write_metadata_to(&media, &with_chapters).expect("write chapters");
        let read = read_metadata_from(&media).expect("read back with chapters");
        assert!(read.chapters_available, "chapters should be readable now");
        assert_eq!(read.chapters.len(), 2, "both chapters round-tripped");
        assert_eq!(read.chapters[0].title, "Intro");
        assert_eq!(read.chapters[1].title, "Main");
        // Tags survived the chapter remux (`-map_metadata 0`).
        assert_eq!(read.title.as_deref(), Some("Episode 12"));
    }
}
