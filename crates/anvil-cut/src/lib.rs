//! # anvil-cut
//!
//! Non-destructive silence and filler cutting (03 §5). Produces a cut-list (an EDL of kept
//! segments) from VAD-negative runs and ASR word timestamps — silence runs shortened to a
//! target gap, disfluencies (`um`/`uh`/…) removed with confidence + padding — then applies
//! it at render time with equal-power crossfades, never cutting inside music segments or
//! below a natural floor. The transcript/review UI (04 §S2 Transcript tab) drives this; the
//! EDL lives in `anvil_project`. The M3 cut lane fills in `silence`, `filler`, and `apply`.
//!
//! ## Pipeline
//!
//! ```text
//! (transcript words + silence runs) --plan--> CutPlan --to_edl--> Edl --apply--> AudioBuffer
//! ```
//!
//! [`plan`] is pure analysis: it emits a [`CutPlan`] (data — a list of accept/reject [`Cut`]s),
//! never touching audio. [`to_edl`] turns the *accepted* cuts into an [`Edl`] of kept
//! segments, and [`apply`] renders that EDL against the decoded [`AudioBuffer`] with an
//! equal-power crossfade at every join (03 §5: "60 ms equal-power crossfades"). Nothing is
//! destructive: the plan and EDL are serializable side-data; the audio is only ever read.
//!
//! ## Silence input (03 §1 "Silence map" / §5)
//!
//! Silence runs are VAD-negative spans. The production path reuses the analysis pass:
//! `anvil_dsp::AnalysisReport::silence_runs` (a `Vec<anvil_dsp::SilenceRun { start, end }>`)
//! maps field-for-field onto [`TimeRange`], so callers build a [`SilenceInput`] with
//! `SilenceInput::from_runs(report.silence_runs.iter().map(|r| (r.start, r.end)))`. To keep
//! this crate free of an `anvil-dsp` dependency (and to make it usable/testable standalone),
//! [`detect_silence`] provides a simple in-crate energy VAD that produces the same
//! [`TimeRange`] runs. Either source feeds [`plan`] identically.
//!
//! ## Contract note — ASR words
//!
//! The published contract consumes `anvil_asr::Transcript { language, words, segments }`, but
//! the M3 `anvil_asr` skeleton has landed only `language` + `segments` (word-level
//! timestamps/confidence land later in that crate). Rather than reach across crate
//! boundaries, [`plan`] takes `&anvil_asr::Transcript` (for `language` → lexicon selection)
//! plus an explicit `&[Word]` slice mirroring the contract's `{text, start, end,
//! confidence}` word shape. When `anvil_asr::Transcript` grows its `words` field, the
//! `&[Word]` parameter folds into the transcript with no change to the cutting logic.

use serde::{Deserialize, Serialize};

use anvil_asr::Transcript;
use anvil_project::edl::Edl;

mod filler;
mod render;
mod silence;

pub use render::{apply, apply_with_crossfade, DEFAULT_CROSSFADE_SECS};
pub use silence::detect_silence;

/// A half-open time span `[start, end)` in seconds. Used for silence runs and music/protected
/// regions. Structurally identical to `anvil_dsp::SilenceRun`, so analysis output maps
/// straight in (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeRange {
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
}

impl TimeRange {
    /// A span `[start, end)`.
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    /// Duration in seconds, clamped to non-negative.
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }

    /// Whether `t` lies within `[start, end)`.
    pub fn contains(&self, t: f64) -> bool {
        t >= self.start && t < self.end
    }

    /// Whether this span overlaps `other` (touching endpoints do not count).
    pub fn overlaps(&self, other: &TimeRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

impl From<(f64, f64)> for TimeRange {
    fn from((start, end): (f64, f64)) -> Self {
        Self { start, end }
    }
}

/// One ASR word with timing and confidence — the filler cutter's input (03 §5: "from ASR
/// word timestamps"). Mirrors the contract's `{text, start, end, confidence}`; see the crate
/// docs for why it is a dedicated type rather than a field on `anvil_asr::Transcript` today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// The recognized token, verbatim from ASR (may carry casing/punctuation — normalized
    /// internally before lexicon matching).
    pub text: String,
    /// Word start time in seconds.
    pub start: f64,
    /// Word end time in seconds.
    pub end: f64,
    /// ASR confidence in `0..=1`. Fillers below [`CutOptions::filler_min_confidence`] are
    /// left in (03 §5: "confidence ≥ 0.85").
    pub confidence: f32,
}

/// What a [`Cut`] removes. Serializes to `"silence"` / `"filler"` (03 §5, snake_case
/// contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutKind {
    /// A shortened VAD-negative gap.
    Silence,
    /// A removed disfluency (`um`, `uh`, …).
    Filler,
}

impl CutKind {
    /// The wire string (`"silence"` / `"filler"`).
    pub fn as_str(self) -> &'static str {
        match self {
            CutKind::Silence => "silence",
            CutKind::Filler => "filler",
        }
    }
}

/// One proposed removal, as an accept/reject row in the review UI (03 §5, 04 §S2). Spans the
/// half-open source range `[start, end)` in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    /// Removal start in source seconds.
    pub start: f64,
    /// Removal end in source seconds.
    pub end: f64,
    /// Whether this cut trims silence or a filler word.
    pub kind: CutKind,
    /// Human-readable label for the review row (e.g. `"um"`, `"silence 2.3s→0.7s"`).
    pub label: String,
    /// Whether this cut is applied. The review UI toggles it; [`to_edl`] only removes
    /// accepted cuts. Fresh plans accept every cut (bulk "apply safe set", 03 §5).
    pub accepted: bool,
}

impl Cut {
    /// Duration removed, in seconds (clamped non-negative).
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }

    fn range(&self) -> TimeRange {
        TimeRange::new(self.start, self.end)
    }
}

/// The full cut-list for one source: the ordered [`Cut`]s plus the source timeline length
/// the plan applies to.
///
/// `source_duration` is the length of the source timeline in seconds; [`to_edl`] needs it to
/// emit the trailing kept segment. [`plan`] infers it from the last event end
/// (`silence`/`words`); when the true media length is known (e.g.
/// `AnalysisReport::duration_secs`), set it via [`CutPlan::with_source_duration`] so no
/// trailing audio is dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CutPlan {
    /// The proposed cuts, in timeline order.
    pub cuts: Vec<Cut>,
    /// Total source timeline length in seconds.
    pub source_duration: f64,
}

impl CutPlan {
    /// Total seconds removed across the *accepted* cuts.
    pub fn removed_secs(&self) -> f64 {
        self.cuts
            .iter()
            .filter(|c| c.accepted)
            .map(Cut::duration)
            .sum()
    }

    /// Override the source timeline length (e.g. with the exact decoded duration) so the
    /// trailing kept segment reaches the real end of media.
    pub fn with_source_duration(mut self, secs: f64) -> Self {
        self.source_duration = secs.max(self.source_duration_floor());
        self
    }

    /// The smallest duration that still covers every cut (so a caller-set duration can never
    /// truncate a cut mid-removal).
    fn source_duration_floor(&self) -> f64 {
        self.cuts.iter().map(|c| c.end).fold(0.0, f64::max)
    }
}

/// The silence side of the cut input: VAD-negative runs to consider, plus the protection
/// context that keeps §5 promises — never cut inside music, protect chapter-boundary
/// silences.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SilenceInput {
    /// VAD-negative runs (from `anvil_dsp` `silence_runs` or [`detect_silence`]).
    pub runs: Vec<TimeRange>,
    /// Music (or music+speech bed) segments — silence is never cut where it overlaps one
    /// (03 §5: "never inside music segments"; 03 §1 speech/music segmentation).
    pub music: Vec<TimeRange>,
    /// Chapter-boundary times (seconds). A silence run straddling one is protected (03 §5:
    /// "chapter-boundary silences protected").
    pub protected_times: Vec<f64>,
}

impl SilenceInput {
    /// Build from silence runs alone (no music/chapter context).
    pub fn from_runs(runs: impl IntoIterator<Item = impl Into<TimeRange>>) -> Self {
        Self {
            runs: runs.into_iter().map(Into::into).collect(),
            music: Vec::new(),
            protected_times: Vec::new(),
        }
    }
}

/// Tunables for [`plan`]. Defaults are the 03 §5 values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CutOptions {
    /// Only silence runs at least this long are shortened (03 §5 default 1.5 s).
    pub min_gap: f64,
    /// Length a shortened silence run is reduced to (03 §5 default 0.7 s).
    pub target_gap: f64,
    /// Hard floor on kept silence — never leave a gap shorter than this "unnatural" length
    /// (03 §5: "never cut below 0.4 s"). Clamps `target_gap` up if a caller sets it lower.
    pub silence_floor: f64,
    /// Padding added around a filler word before cutting (03 §5: "padded ±40 ms").
    pub filler_pad: f64,
    /// Minimum ASR confidence for a filler to be cut (03 §5: "confidence ≥ 0.85").
    pub filler_min_confidence: f32,
    /// `aggressive` mode also treats hedges (`you know`, `like`) as fillers (03 §5).
    pub aggressive: bool,
    /// Merge cuts whose padded edges fall within this gap so a lone breath between two cuts
    /// is swallowed rather than left as a "gasp-cut" (03 §5: "breath-adjacent trims merge").
    pub merge_gap: f64,
    /// Emit silence cuts.
    pub cut_silence: bool,
    /// Emit filler cuts.
    pub cut_fillers: bool,
}

impl Default for CutOptions {
    fn default() -> Self {
        Self {
            min_gap: 1.5,
            target_gap: 0.7,
            silence_floor: 0.4,
            filler_pad: 0.040,
            filler_min_confidence: 0.85,
            aggressive: false,
            merge_gap: 0.10,
            cut_silence: true,
            cut_fillers: true,
        }
    }
}

/// Plan the cuts for one source (03 §5). Deterministic: identical inputs ⇒ identical
/// [`CutPlan`].
///
/// - `transcript` supplies `language` (selects the filler lexicon).
/// - `words` are the ASR word timestamps the filler cutter reads (see crate docs on why this
///   is a separate parameter from `transcript` today).
/// - `silence` carries the VAD-negative runs plus music/chapter protection context.
/// - `opts` tunes the §5 thresholds.
///
/// Silence runs `≥ min_gap` are shortened to `target_gap` (never inside `music`, never
/// straddling a `protected_times` boundary, never below `silence_floor`). Fillers matching
/// the language lexicon with confidence `≥ filler_min_confidence` are removed with `±pad`.
/// Cuts within `merge_gap` are merged (breath protection). Every cut starts `accepted`.
pub fn plan(
    transcript: &Transcript,
    words: &[Word],
    silence: &SilenceInput,
    opts: &CutOptions,
) -> CutPlan {
    let mut cuts: Vec<Cut> = Vec::new();

    if opts.cut_silence {
        cuts.extend(silence::plan_silence_cuts(silence, opts));
    }
    if opts.cut_fillers {
        cuts.extend(filler::plan_filler_cuts(
            &transcript.language,
            words,
            silence,
            opts,
        ));
    }

    // Timeline order, then merge adjacent removals so a breath sandwiched between cuts goes
    // with them (03 §5). NaN-free by construction (finite times), so total ordering holds.
    cuts.sort_by(|a, b| a.start.total_cmp(&b.start));
    let cuts = merge_cuts(cuts, opts.merge_gap);

    let source_duration = cuts
        .iter()
        .map(|c| c.end)
        .chain(words.iter().map(|w| w.end))
        .chain(silence.runs.iter().map(|r| r.end))
        .fold(0.0, f64::max);

    CutPlan {
        cuts,
        source_duration,
    }
}

/// Merge cuts whose edges fall within `merge_gap`. A silence cut absorbs an abutting filler
/// (and the breath between them); the merged span keeps the structural [`CutKind::Silence`]
/// where either side is silence.
fn merge_cuts(cuts: Vec<Cut>, merge_gap: f64) -> Vec<Cut> {
    let mut merged: Vec<Cut> = Vec::with_capacity(cuts.len());
    for cut in cuts {
        match merged.last_mut() {
            Some(prev) if cut.start <= prev.end + merge_gap => {
                prev.end = prev.end.max(cut.end);
                prev.accepted = prev.accepted && cut.accepted;
                if cut.kind == CutKind::Silence {
                    prev.kind = CutKind::Silence;
                }
                if prev.label != cut.label {
                    prev.label = format!("{} + {}", prev.label, cut.label);
                }
            }
            _ => merged.push(cut),
        }
    }
    merged
}

/// Turn a [`CutPlan`] into an [`Edl`] of kept segments over one source (03 §5). Only
/// `accepted` cuts are removed; the remaining timeline `[0, source_duration)` becomes an
/// alternating run of kept/cut [`anvil_project::edl::Segment`]s (kept segments render
/// back-to-back). The single source is index `0`; the caller sets its real path via
/// `edl.sources[0].path` (see [`apply`], which reads by index only).
pub fn to_edl(plan: &CutPlan) -> Edl {
    use anvil_project::edl::{EdlSource, Segment};

    let duration = plan.source_duration.max(0.0);

    // Accepted cuts, clamped to the timeline and coalesced so overlaps never produce a
    // backwards segment.
    let mut removals: Vec<TimeRange> = plan
        .cuts
        .iter()
        .filter(|c| c.accepted)
        .map(|c| TimeRange::new(c.start.clamp(0.0, duration), c.end.clamp(0.0, duration)))
        .filter(|r| r.duration() > 0.0)
        .collect();
    removals.sort_by(|a, b| a.start.total_cmp(&b.start));

    let mut coalesced: Vec<TimeRange> = Vec::with_capacity(removals.len());
    for r in removals {
        match coalesced.last_mut() {
            Some(prev) if r.start <= prev.end => prev.end = prev.end.max(r.end),
            _ => coalesced.push(r),
        }
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut cursor = 0.0_f64;
    for cut in &coalesced {
        if cut.start > cursor {
            segments.push(Segment::kept(0, cursor, cut.start));
        }
        segments.push(Segment::cut(0, cut.start, cut.end));
        cursor = cut.end;
    }
    if cursor < duration {
        segments.push(Segment::kept(0, cursor, duration));
    }

    let mut edl = Edl::new(vec![EdlSource::new("source")]);
    edl.segments = segments;
    edl
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start: f64, end: f64, confidence: f32) -> Word {
        Word {
            text: text.into(),
            start,
            end,
            confidence,
        }
    }

    /// EDL golden test: known words + one silence run → known kept segments (03 §5).
    #[test]
    fn edl_golden_kept_segments() {
        let transcript = Transcript {
            language: "en".into(),
            words: Vec::new(),
            segments: Vec::new(),
        };
        // One clean "um" at 5.0–5.3 s and a 3 s silence run at 10.0–13.0 s.
        let words = vec![word("um", 5.0, 5.3, 0.95)];
        let silence = SilenceInput::from_runs([(10.0, 13.0)]);
        let opts = CutOptions::default();

        let plan = plan(&transcript, &words, &silence, &opts).with_source_duration(20.0);

        // Filler cut: 5.0..5.3 padded ±0.04 → 4.96..5.34.
        // Silence cut: 3 s run kept to 0.7 s, centered → cut 10.35..12.65.
        assert_eq!(plan.cuts.len(), 2);
        assert_eq!(plan.cuts[0].kind, CutKind::Filler);
        assert!((plan.cuts[0].start - 4.96).abs() < 1e-9);
        assert!((plan.cuts[0].end - 5.34).abs() < 1e-9);
        assert_eq!(plan.cuts[1].kind, CutKind::Silence);
        assert!((plan.cuts[1].start - 10.35).abs() < 1e-9);
        assert!((plan.cuts[1].end - 12.65).abs() < 1e-9);

        let edl = to_edl(&plan);
        let kept: Vec<(f64, f64)> = edl
            .kept_ranges()
            .map(|s| (s.source_in, s.source_out))
            .collect();
        let expect = [(0.0, 4.96), (5.34, 10.35), (12.65, 20.0)];
        assert_eq!(kept.len(), expect.len());
        for (got, want) in kept.iter().zip(expect.iter()) {
            assert!((got.0 - want.0).abs() < 1e-9, "{got:?} vs {want:?}");
            assert!((got.1 - want.1).abs() < 1e-9, "{got:?} vs {want:?}");
        }
        // Kept total = 4.96 + 5.01 + 7.35 = 17.32.
        assert!((edl.total_duration() - 17.32).abs() < 1e-9);
    }

    /// Only accepted cuts are removed (03 §5: "only accepted cuts removed").
    #[test]
    fn rejected_cuts_stay_in_the_edl() {
        let mut plan = CutPlan {
            cuts: vec![Cut {
                start: 2.0,
                end: 3.0,
                kind: CutKind::Filler,
                label: "uh".into(),
                accepted: false,
            }],
            source_duration: 10.0,
        };
        // Rejected → whole timeline kept as one segment.
        let edl = to_edl(&plan);
        assert_eq!(edl.total_duration(), 10.0);
        assert_eq!(edl.kept_ranges().count(), 1);

        // Accept it → two kept segments around the hole.
        plan.cuts[0].accepted = true;
        let edl = to_edl(&plan);
        assert_eq!(edl.kept_ranges().count(), 2);
        assert!((edl.total_duration() - 9.0).abs() < 1e-9);
    }

    /// Determinism: identical inputs ⇒ identical plan (06 §2 determinism gate).
    #[test]
    fn plan_is_deterministic() {
        let transcript = Transcript {
            language: "en".into(),
            words: Vec::new(),
            segments: Vec::new(),
        };
        let words = vec![word("um", 1.0, 1.2, 0.9), word("uh", 4.0, 4.2, 0.95)];
        let silence = SilenceInput::from_runs([(6.0, 8.0)]);
        let opts = CutOptions::default();
        let a = plan(&transcript, &words, &silence, &opts);
        let b = plan(&transcript, &words, &silence, &opts);
        assert_eq!(a, b);
    }

    /// Silence inside a music segment is never cut (03 §5).
    #[test]
    fn music_segments_protect_silence() {
        let transcript = Transcript::default();
        let silence = SilenceInput {
            runs: vec![TimeRange::new(2.0, 5.0)],
            music: vec![TimeRange::new(0.0, 10.0)],
            protected_times: Vec::new(),
        };
        let plan = plan(&transcript, &[], &silence, &CutOptions::default());
        assert!(plan.cuts.is_empty());
    }

    /// A chapter-boundary silence is protected (03 §5).
    #[test]
    fn chapter_boundary_protects_silence() {
        let transcript = Transcript::default();
        let silence = SilenceInput {
            runs: vec![TimeRange::new(2.0, 5.0)],
            music: Vec::new(),
            protected_times: vec![3.5],
        };
        let plan = plan(&transcript, &[], &silence, &CutOptions::default());
        assert!(plan.cuts.is_empty());
    }
}
