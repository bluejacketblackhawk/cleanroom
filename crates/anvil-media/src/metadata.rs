//! Metadata / chapters (ADR-005 §Metadata/chapters).
//!
//! Standard tags and cover art round-trip through [`lofty`] (MIT/Apache): ID3v2.3/2.4, MP4
//! atoms, and Vorbis comments. [`TagEditor`] wraps `lofty::file::BoundTaggedFile` rather than
//! a hand-rolled struct — that's what makes the **round-trip rule** ("never drop tags the
//! user didn't touch") hold structurally instead of by discipline: opening a file reads every
//! tag verbatim, setters mutate only the one item they name, and saving re-serializes the
//! *whole* tag object, untouched items included. See `tests/metadata.rs` for the round-trip
//! proof this rule demands.
//!
//! **Chapters are not a lofty feature.** As of lofty 0.24 there is no ID3v2 CHAP/CTOC frame
//! support and no MP4 chapter-atom support anywhere in the crate (verified against its
//! source — grepping for "chapter" turns up exactly one unrelated `// TODO` in the musepack
//! reader). So, per ADR-005's fallback plan for "MP4 chapters + M4B via ffmpeg
//! `-map_metadata`/ffmetadata where lofty can't", chapters for *every* container — including
//! ID3v2 CHAP/CTOC, which ffmpeg's MP3 muxer does write from an ffmetadata chapter list —
//! go through [`write_chapters`]/[`read_chapters`] instead. [`write_chapters`] always pairs
//! `-map_metadata 0 -map_chapters 1` with `-c copy`: metadata comes from the *original* file
//! (input 0), only chapters come from the synthetic ffmetadata file (input 1), and no stream
//! is re-encoded. That is what keeps it from ever clobbering tags lofty already wrote.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use lofty::config::WriteOptions;
use lofty::file::{BoundTaggedFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::Accessor;
use lofty::tag::{Tag, TagType};

use crate::error::MediaError;
use crate::sidecar::{push_tail, FfmpegSidecar};

/// One audiobook/podcast chapter marker. `start_ms`/`end_ms` are offsets from the start of
/// the file; `end_ms` should equal the next chapter's `start_ms` (or the file's duration for
/// the last chapter) — ffmpeg's `ffmetadata` format requires an explicit end for every
/// chapter, there is no "open-ended, runs to the next marker" shorthand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    pub title: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Cover art: raw image bytes plus MIME type (`image/jpeg`, `image/png`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArt {
    pub mime: String,
    pub data: Vec<u8>,
}

/// An open, editable tag on a media file, backed by `lofty::file::BoundTaggedFile` (see the
/// module docs for why that's what gives the round-trip guarantee). Read fields with the
/// getters, change only the ones you mean to with the setters, then [`TagEditor::save`].
///
/// If the file has no tag of its container's primary type yet, one is created lazily on first
/// write (an empty tag is never saved — see lofty's [`WriteOptions`] behavior — so opening a
/// file and saving it back unchanged is a no-op).
pub struct TagEditor {
    file: BoundTaggedFile<std::fs::File>,
}

impl TagEditor {
    /// Open `path` for reading and writing its primary tag type (ID3v2 for MP3, MP4 atoms for
    /// m4a/m4b, Vorbis comments for ogg/opus/flac, …).
    pub fn open(path: &Path) -> Result<Self, MediaError> {
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        // Tolerate junk frames (e.g. a malformed `TYER` like "38-10-16" that real archival
        // files carry): discard the bad item and keep reading the rest, rather than failing
        // the entire file. A genuinely unreadable container still errors.
        let file = BoundTaggedFile::read_from(
            handle,
            lofty::config::ParseOptions::new().parsing_mode(lofty::config::ParsingMode::Relaxed),
        )?;
        Ok(Self { file })
    }

    fn primary_tag_type(&self) -> TagType {
        self.file.primary_tag_type()
    }

    /// Get-or-create the primary tag, for setters. Creating one does not touch the file until
    /// [`Self::save`] — and an empty tag that's never populated is never written.
    fn primary_tag_mut(&mut self) -> &mut Tag {
        if self.file.primary_tag().is_none() {
            self.file.insert_tag(Tag::new(self.primary_tag_type()));
        }
        self.file
            .primary_tag_mut()
            .expect("just inserted the primary tag")
    }

    pub fn title(&self) -> Option<String> {
        self.file
            .primary_tag()
            .and_then(|t| t.title())
            .map(Into::into)
    }

    pub fn set_title(&mut self, value: impl Into<String>) {
        self.primary_tag_mut().set_title(value.into());
    }

    pub fn artist(&self) -> Option<String> {
        self.file
            .primary_tag()
            .and_then(|t| t.artist())
            .map(Into::into)
    }

    pub fn set_artist(&mut self, value: impl Into<String>) {
        self.primary_tag_mut().set_artist(value.into());
    }

    pub fn album(&self) -> Option<String> {
        self.file
            .primary_tag()
            .and_then(|t| t.album())
            .map(Into::into)
    }

    pub fn set_album(&mut self, value: impl Into<String>) {
        self.primary_tag_mut().set_album(value.into());
    }

    pub fn genre(&self) -> Option<String> {
        self.file
            .primary_tag()
            .and_then(|t| t.genre())
            .map(Into::into)
    }

    pub fn set_genre(&mut self, value: impl Into<String>) {
        self.primary_tag_mut().set_genre(value.into());
    }

    pub fn comment(&self) -> Option<String> {
        self.file
            .primary_tag()
            .and_then(|t| t.comment())
            .map(Into::into)
    }

    pub fn set_comment(&mut self, value: impl Into<String>) {
        self.primary_tag_mut().set_comment(value.into());
    }

    /// Track (episode) number.
    pub fn track(&self) -> Option<u32> {
        self.file.primary_tag().and_then(|t| t.track())
    }

    pub fn set_track(&mut self, value: u32) {
        self.primary_tag_mut().set_track(value);
    }

    /// Release date/timestamp, ISO-8601-ish (lofty's [`lofty::tag::items::Timestamp`] format,
    /// e.g. `"2026-07-13"`). Kept as a plain string here so lofty's timestamp type doesn't
    /// leak into this crate's public API.
    pub fn date(&self) -> Option<String> {
        self.file
            .primary_tag()
            .and_then(|t| t.date())
            .map(|d| d.to_string())
    }

    /// Parses `value` as a lofty [`lofty::tag::items::Timestamp`]; returns
    /// [`MediaError::Metadata`] if it isn't one (year, or `YYYY-MM-DD`, or a full ISO
    /// timestamp).
    pub fn set_date(&mut self, value: &str) -> Result<(), MediaError> {
        let ts: lofty::tag::items::Timestamp = value
            .parse()
            .map_err(|_| MediaError::Metadata(format!("not a valid date/timestamp: {value:?}")))?;
        self.primary_tag_mut().set_date(ts);
        Ok(())
    }

    /// The first cover-art picture, if any.
    pub fn cover_art(&self) -> Option<CoverArt> {
        let picture = self.file.primary_tag()?.pictures().first()?;
        Some(CoverArt {
            mime: picture
                .mime_type()
                .map(mime_to_string)
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            data: picture.data().to_vec(),
        })
    }

    /// Replace all cover art with a single front-cover picture.
    pub fn set_cover_art(&mut self, cover: CoverArt) {
        let picture = Picture::unchecked(cover.data)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::from_str(&cover.mime))
            .build();
        let tag = self.primary_tag_mut();
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(picture);
    }

    pub fn remove_cover_art(&mut self) {
        self.primary_tag_mut()
            .remove_picture_type(PictureType::CoverFront);
    }

    /// Save with ID3v2.4 (lofty's default) where applicable; MP4 atoms and Vorbis comments
    /// have no version split and are unaffected by this choice.
    pub fn save(&mut self) -> Result<(), MediaError> {
        self.file.save(WriteOptions::default())?;
        Ok(())
    }

    /// Save with legacy ID3v2.3 framing instead of 2.4 (some older podcast apps/hardware
    /// still expect 2.3). No-op for tag types other than ID3v2.
    pub fn save_id3v23(&mut self) -> Result<(), MediaError> {
        let mut options = WriteOptions::default();
        options = options.use_id3v23(true);
        self.file.save(options)?;
        Ok(())
    }
}

fn mime_to_string(mime: &MimeType) -> String {
    match mime {
        MimeType::Png => "image/png".to_string(),
        MimeType::Jpeg => "image/jpeg".to_string(),
        MimeType::Tiff => "image/tiff".to_string(),
        MimeType::Bmp => "image/bmp".to_string(),
        MimeType::Gif => "image/gif".to_string(),
        MimeType::Unknown(s) => s.clone(),
        _ => "application/octet-stream".to_string(),
    }
}

/// Read chapters from `path` by parsing the `Chapters:` block ffmpeg prints in its `-i`
/// banner (the same banner [`FfmpegSidecar::probe`] parses for stream facts). Returns an
/// empty `Vec` if the file has no chapters.
pub fn read_chapters(sidecar: &FfmpegSidecar, path: &Path) -> Result<Vec<Chapter>, MediaError> {
    Ok(parse_chapters_banner(&sidecar.banner(path)?))
}

/// Embed `chapters` into `path` in place: remux to a temp file with `-map_metadata 0
/// -map_chapters 1 -c copy` (original tags kept, chapters taken from a synthetic ffmetadata
/// file, no stream re-encoded — see the module docs), then atomically replace the original.
/// An empty `chapters` slice clears any existing chapters.
pub fn write_chapters(
    sidecar: &FfmpegSidecar,
    path: &Path,
    chapters: &[Chapter],
) -> Result<(), MediaError> {
    let meta_path = write_ffmetadata_chapters(chapters)?;
    let out_path = temp_sibling_path(path);

    let result = (|| -> Result<(), MediaError> {
        let output = Command::new(sidecar.binary())
            .args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
            .arg("-i")
            .arg(path)
            .arg("-i")
            .arg(&meta_path)
            .args(["-map_metadata", "0", "-map_chapters", "1", "-map", "0"])
            .args(["-c", "copy"])
            .arg(&out_path)
            .output()?;

        if !output.status.success() {
            let mut tail = String::new();
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                push_tail(&mut tail, line);
            }
            return Err(MediaError::SidecarFailed(format!(
                "ffmpeg exited with {}: {}",
                output.status,
                tail.trim()
            )));
        }
        std::fs::rename(&out_path, path)?;
        Ok(())
    })();

    let _ = std::fs::remove_file(&meta_path);
    if result.is_err() {
        let _ = std::fs::remove_file(&out_path);
    }
    result
}

/// A path next to `path` to write a remux result to before the atomic rename in
/// [`write_chapters`] — same directory so the rename can't cross filesystems.
fn temp_sibling_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let marker = format!(".anvil-tmp-{}", std::process::id());
    // Keep the original extension LAST so ffmpeg can infer the output muxer from it
    // (`foo.mp3` -> `foo.anvil-tmp-123.mp3`, not `foo.mp3.anvil-tmp-123`).
    let name = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}{marker}.{ext}"),
        None => format!("{stem}{marker}"),
    };
    path.with_file_name(name)
}

/// Write `chapters` to a temporary `ffmetadata`-format file ffmpeg can read via `-i`. Also
/// used by [`crate::encode::FfmpegSidecar::encode_m4b_audiobook`] for the single-pass M4B
/// path. Returns the temp file's path; the caller is responsible for deleting it.
pub(crate) fn write_ffmetadata_chapters(chapters: &[Chapter]) -> Result<PathBuf, MediaError> {
    let path = std::env::temp_dir().join(format!(
        "anvil_chapters_{}_{}.ffmeta",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = std::fs::File::create(&path)?;
    writeln!(file, ";FFMETADATA1")?;
    for chapter in chapters {
        writeln!(file, "[CHAPTER]")?;
        writeln!(file, "TIMEBASE=1/1000")?;
        writeln!(file, "START={}", chapter.start_ms)?;
        writeln!(file, "END={}", chapter.end_ms)?;
        writeln!(file, "title={}", escape_ffmetadata(&chapter.title))?;
    }
    Ok(path)
}

/// Escape `=`, `;`, `#`, `\`, and newlines per the `ffmetadata` format's escaping rules
/// (https://ffmpeg.org/ffmpeg-formats.html#Metadata-2): each is backslash-prefixed so it
/// can't be mistaken for the format's own syntax.
fn escape_ffmetadata(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '=' | ';' | '#' | '\\' | '\n') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Parse the `Chapters:` block out of an ffmpeg `-i` banner:
/// ```text
///   Chapters:
///     Chapter #0:0: start 0.000000, end 1.000000
///       Metadata:
///         title           : Intro
/// ```
fn parse_chapters_banner(banner: &str) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    let mut lines = banner.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("Chapter #") else {
            continue;
        };
        // rest like "0:0: start 0.000000, end 1.000000"
        let Some(times) = rest.split_once("start ") else {
            continue;
        };
        let Some((start_str, end_part)) = times.1.split_once(", end ") else {
            continue;
        };
        let Some(start) = start_str.trim().parse::<f64>().ok() else {
            continue;
        };
        let Some(end) = end_part.trim().trim_end_matches(':').parse::<f64>().ok() else {
            continue;
        };

        // The title (if any) is on a later "title : X" line, before the next "Chapter #" or
        // top-level section. ffmpeg indents metadata under "Metadata:"; scan forward but stop
        // at the next chapter/stream so we don't attribute the wrong title.
        let mut title = String::new();
        while let Some(next) = lines.peek() {
            let next_trimmed = next.trim();
            if next_trimmed.starts_with("Chapter #") || next_trimmed.starts_with("Stream #") {
                break;
            }
            if let Some((key, value)) = next_trimmed.split_once(':') {
                if key.trim() == "title" {
                    title = value.trim().to_string();
                    lines.next();
                    break;
                }
            }
            lines.next();
        }

        chapters.push(Chapter {
            title,
            start_ms: (start * 1000.0).round() as u64,
            end_ms: (end * 1000.0).round() as u64,
        });
    }

    chapters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chapters_from_banner() {
        let banner = "\
Input #0, mp3, from 'x.mp3':
  Duration: 00:00:03.02, start: 0.023021, bitrate: 129 kb/s
  Chapters:
    Chapter #0:0: start 0.000000, end 1.000000
      Metadata:
        title           : Intro
    Chapter #0:1: start 1.000000, end 3.000000
      Metadata:
        title           : Body
  Stream #0:0: Audio: mp3, 48000 Hz, stereo, fltp, 128 kb/s
";
        let chapters = parse_chapters_banner(banner);
        assert_eq!(
            chapters,
            vec![
                Chapter {
                    title: "Intro".into(),
                    start_ms: 0,
                    end_ms: 1000
                },
                Chapter {
                    title: "Body".into(),
                    start_ms: 1000,
                    end_ms: 3000
                },
            ]
        );
    }

    #[test]
    fn no_chapters_block_yields_empty() {
        let banner = "Input #0, wav, from 'x.wav':\n  Duration: 00:00:01.00\n";
        assert!(parse_chapters_banner(banner).is_empty());
    }

    #[test]
    fn escapes_special_ffmetadata_chars() {
        assert_eq!(escape_ffmetadata("a=b;c#d\\e"), "a\\=b\\;c\\#d\\\\e");
    }

    /// Build a tiny MP3 (ID3v2.3 tag + one MPEG-1 Layer III frame) whose `TYER` frame holds a
    /// malformed year — the exact junk real 1938-era archival files carry.
    fn write_mp3_with_bad_tyer(path: &Path) {
        fn text_frame(out: &mut Vec<u8>, id: &[u8; 4], text: &str) {
            let mut body = vec![0u8]; // ISO-8859-1 encoding byte
            body.extend_from_slice(text.as_bytes());
            out.extend_from_slice(id);
            out.extend_from_slice(&(body.len() as u32).to_be_bytes()); // v2.3: plain big-endian
            out.extend_from_slice(&[0x00, 0x00]); // frame flags
            out.extend_from_slice(&body);
        }

        let mut frames = Vec::new();
        text_frame(&mut frames, b"TIT2", "Night Without End");
        text_frame(&mut frames, b"TYER", "38-10-16"); // <- malformed year

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ID3");
        bytes.extend_from_slice(&[0x03, 0x00, 0x00]); // v2.3, no header flags
        let n = frames.len() as u32; // syncsafe 28-bit tag size
        bytes.extend_from_slice(&[
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]);
        bytes.extend_from_slice(&frames);

        // One MPEG-1 Layer III frame @128 kbps/44.1 kHz = 417 bytes (valid header + zeroed
        // body): enough for lofty to recognize the file as MP3.
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        // Two consecutive frames so lofty can confirm the sync (a lone frame reads as invalid).
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(&frame);

        std::fs::write(path, &bytes).unwrap();
    }

    #[test]
    fn read_tolerates_malformed_tyer_frame() {
        let dir = std::env::temp_dir().join(format!("anvil-md-badtyer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.mp3");
        write_mp3_with_bad_tyer(&path);

        // Before the Relaxed fix this errored on the whole file; now the junk TYER is dropped
        // and the good title still reads.
        let editor = TagEditor::open(&path).expect("a malformed TYER must not fail the read");
        assert_eq!(editor.title().as_deref(), Some("Night Without End"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
