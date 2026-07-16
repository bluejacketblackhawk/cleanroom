//! Edit decision list: the timeline of what survives to the rendered output (ADR-008,
//! `cuts.json`). An [`Edl`] references one or more [`EdlSource`] media files by index and
//! carves them into ordered [`Segment`]s, each either kept (rendered) or cut (dropped).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::PROJECT_SCHEMA_VERSION;

/// A point in time, in seconds. `f64` so hour-plus timelines stay sample-accurate (a
/// `f32` loses sub-millisecond precision well before a typical podcast's length).
pub type Seconds = f64;

/// One source media file referenced by segments in an [`Edl`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdlSource {
    /// Path to the source file, typically relative to the `.anvilproj` folder.
    pub path: PathBuf,
    /// Content hash (sha256 hex digest), used to detect external modification so the UI
    /// can flag staleness and re-analyze (ADR-008 "Enforce: source file content
    /// hashes"). `None` until the first analysis pass computes it.
    pub content_hash: Option<String>,
}

impl EdlSource {
    /// A source with no hash computed yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            content_hash: None,
        }
    }
}

/// A contiguous slice of one source, tagged kept or cut. Segments are ordered in
/// timeline order within [`Edl::segments`]; kept segments render back-to-back, cut
/// segments are silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Index into [`Edl::sources`] this segment cuts from.
    pub source_index: usize,
    /// In point within the source, in seconds.
    pub source_in: Seconds,
    /// Out point within the source, in seconds. Expected `>= source_in`; a segment with
    /// `source_out <= source_in` has zero duration and contributes nothing.
    pub source_out: Seconds,
    /// Whether this segment survives to the rendered output.
    pub kept: bool,
}

impl Segment {
    /// A kept segment spanning `[source_in, source_out)` of `source_index`.
    pub fn kept(source_index: usize, source_in: Seconds, source_out: Seconds) -> Self {
        Self {
            source_index,
            source_in,
            source_out,
            kept: true,
        }
    }

    /// A cut segment spanning `[source_in, source_out)` of `source_index`.
    pub fn cut(source_index: usize, source_in: Seconds, source_out: Seconds) -> Self {
        Self {
            source_index,
            source_in,
            source_out,
            kept: false,
        }
    }

    /// Duration of this segment, clamped to non-negative.
    pub fn duration(&self) -> Seconds {
        (self.source_out - self.source_in).max(0.0)
    }
}

/// Edit decision list: an ordered timeline of segments cut from one or more sources
/// (`cuts.json` inside the `.anvilproj` folder, ADR-008).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edl {
    pub schema_version: u32,
    pub sources: Vec<EdlSource>,
    pub segments: Vec<Segment>,
}

impl Edl {
    /// A fresh EDL over `sources` with no segments cut yet.
    pub fn new(sources: Vec<EdlSource>) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            sources,
            segments: Vec::new(),
        }
    }

    /// Segments that survive to the render, in timeline order.
    pub fn kept_ranges(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.kept)
    }

    /// Total duration of the rendered output: the sum of kept segment durations. Cut
    /// segments (and any gaps not covered by a segment at all) don't count.
    pub fn total_duration(&self) -> Seconds {
        self.kept_ranges().map(Segment::duration).sum()
    }
}

impl Default for Edl {
    /// An empty EDL: no sources, no segments, zero duration.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edl() -> Edl {
        let mut edl = Edl::new(vec![EdlSource::new("episode.wav")]);
        edl.segments = vec![
            Segment::kept(0, 0.0, 10.0),
            Segment::cut(0, 10.0, 12.5),
            Segment::kept(0, 12.5, 20.0),
        ];
        edl
    }

    #[test]
    fn total_duration_sums_only_kept_segments() {
        let edl = sample_edl();
        assert_eq!(edl.total_duration(), 10.0 + 7.5);
    }

    #[test]
    fn kept_ranges_skips_cut_segments() {
        let edl = sample_edl();
        let kept: Vec<_> = edl.kept_ranges().collect();
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|s| s.kept));
    }

    #[test]
    fn empty_edl_has_zero_duration() {
        assert_eq!(Edl::default().total_duration(), 0.0);
    }

    #[test]
    fn edl_roundtrips_json() {
        let edl = sample_edl();
        let json = serde_json::to_string_pretty(&edl).unwrap();
        let back: Edl = serde_json::from_str(&json).unwrap();
        assert_eq!(edl, back);
    }

    #[test]
    fn segment_duration_never_negative() {
        let backwards = Segment::kept(0, 5.0, 2.0);
        assert_eq!(backwards.duration(), 0.0);
    }
}
