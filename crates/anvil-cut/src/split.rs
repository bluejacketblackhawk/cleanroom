//! Cut-word splitting (handoff/09): find every spoken instance of a user-chosen cut word in
//! the ASR word stream, treat each accepted one as a **split boundary**, and partition the
//! source timeline into parts — the cut word (plus the dead air around it) deleted entirely.
//!
//! ## Pipeline (09 §2–3)
//!
//! ```text
//! (words + silence runs) --detect_cut_words--> Vec<Cut{kind: CutWord}>   (review rows)
//! CutPlan{cut-word + silence/filler cuts} --partition--> SegmentPlan{parts: Vec<Part>}
//! Part.edl --apply_with_edge_fades--> one part's audio
//! ```
//!
//! [`detect_cut_words`] is pure matching: normalized (case-folded, punctuation-stripped)
//! token comparison against the user's phrase, one [`Cut`] per instance. Exact matches with
//! confident words arrive `accepted`; fuzzy matches (edit distance 1 on tokens ≥ 5 chars),
//! low-confidence matches, and matches lying entirely inside a VAD-negative run (whisper's
//! classic hallucination habitat, 09 §2) arrive **unaccepted** — "possible match" rows for
//! the review UI. A `Cut` spans just the matched words; the removal *geometry* — swallowing
//! the surrounding silence, keeping the 09 §3 speech pads — lives in [`partition`] so the
//! review row shows the word where it was said.
//!
//! [`partition`] walks the accepted `CutWord` cuts in timeline order, extends each across
//! every abutting/overlapping silence run, backs off by the speech pads (never shrinking
//! below the word span itself), merges overlapping removals (a doubled cut word is one
//! boundary), and emits one [`Part`] per surviving span. Accepted silence/filler cuts that
//! fall *inside* a part carry into that part's [`Edl`] (splitting composes with the M3
//! cutting features); ones overlapping a boundary removal are dropped — the boundary owns
//! that neighborhood. Parts whose kept audio is shorter than
//! [`SplitOptions::min_part_secs`] are dropped, so "cut word after every script" and "only
//! between scripts" both yield exactly N parts.
//!
//! Everything here is deterministic: identical inputs ⇒ identical matches and identical
//! [`SegmentPlan`] (06 §2).

use serde::{Deserialize, Serialize};

use anvil_project::edl::{Edl, EdlSource, Segment};

use crate::filler::normalize;
use crate::{Cut, CutKind, CutPlan, SilenceInput, TimeRange, Word};

/// Longest silent gap allowed *inside* a multi-word cut phrase (09 §2: "match consecutive
/// words with ≤ 0.5 s inter-word gap").
const MAX_PHRASE_GAP: f64 = 0.5;

/// A silence run and a matched word "touch" within this tolerance when extending the
/// removal region — whisper word edges and VAD edges disagree by a few tens of ms.
const TOUCH_SECS: f64 = 0.05;

/// Minimum normalized token length for fuzzy (edit-distance-1) and phonetic matching
/// (09 §2: "≥ 5 chars" — short tokens would false-positive on ordinary speech).
const MIN_FUZZY_LEN: usize = 5;

/// A word "stands alone" when a silence run ends within this window before it and another
/// begins within it after — the acoustic signature of a cut word spoken by itself between
/// scripts. Isolated non-exact matches are trusted; embedded ones stay review-only.
const ISOLATION_WINDOW: f64 = 0.35;

/// Two ASR tokens merge into one candidate word (whisper splitting "kumquat" into
/// "kum quat") only when the gap between them is at most this.
const MERGE_TOKEN_GAP: f64 = 0.2;

/// How many leading words a part contributes to its name (09 §5: `{first_words}`).
const TITLE_WORDS: usize = 4;

/// Tunables for cut-word detection and partitioning. Defaults are the 09 §2–3 values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitOptions {
    /// The cut word or phrase, verbatim from the user (settings / `--cut-word`). Matched
    /// against [`normalize`]d tokens, so case and attached punctuation never matter.
    pub phrase: String,
    /// Also propose edit-distance-1 matches on tokens ≥ 5 chars ("cumquat" for "kumquat").
    /// Fuzzy matches always arrive unaccepted (09 §2).
    pub fuzzy: bool,
    /// Words below this ASR confidence make the match a "possible match" (unaccepted)
    /// rather than an accepted boundary. Deliberately lower than the filler gate — cut
    /// words are spoken in isolation and the review UI is the real safety net (09 §2).
    pub min_confidence: f32,
    /// Room tone kept *before* each part's first speech, in seconds (09 §3 pre-roll).
    pub speech_pad_pre: f64,
    /// Room tone kept *after* each part's last speech, in seconds (09 §3 post-roll).
    pub speech_pad_post: f64,
    /// Parts with less kept audio than this are dropped (degenerate spans: leading noise
    /// before script 1, trailing tail after a final cut word, doubled cut words).
    pub min_part_secs: f64,
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            phrase: String::new(),
            fuzzy: true,
            min_confidence: 0.6,
            speech_pad_pre: 0.15,
            speech_pad_post: 0.30,
            min_part_secs: 0.5,
        }
    }
}

impl SplitOptions {
    /// Options for `phrase` with every other field at its 09 default.
    pub fn for_phrase(phrase: impl Into<String>) -> Self {
        Self {
            phrase: phrase.into(),
            ..Self::default()
        }
    }

    /// The normalized phrase tokens. Empty if the phrase is blank (no matching happens).
    fn tokens(&self) -> Vec<String> {
        self.phrase
            .split_whitespace()
            .map(normalize)
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// One output segment of a [`SegmentPlan`]: a span of the source timeline between split
/// boundaries, with its own single-source [`Edl`] (kept ranges inside `[start, end]`,
/// minus any interior accepted cuts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    /// 0-based order among the surviving parts.
    pub index: usize,
    /// Part start on the source timeline, in seconds (post-pad boundary).
    pub start: f64,
    /// Part end on the source timeline, in seconds (post-pad boundary).
    pub end: f64,
    /// The part's first few transcribed words, normalized — the `{first_words}` naming
    /// token (09 §5). Empty if no words landed inside the part.
    pub title_words: Vec<String>,
    /// Render this to get the part's audio (single source, index 0; see
    /// [`crate::apply_with_edge_fades`]).
    pub edl: Edl,
}

impl Part {
    /// Duration of the part's kept audio (its EDL total), in seconds.
    pub fn kept_secs(&self) -> f64 {
        self.edl.total_duration()
    }

    /// `{first_words}` joined for display/naming, e.g. `"why nobody holds the"`.
    pub fn title(&self) -> String {
        self.title_words.join(" ")
    }
}

/// The full split of one source: the surviving [`Part`]s in timeline order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentPlan {
    pub parts: Vec<Part>,
    /// Source timeline length the plan was computed over, in seconds.
    pub source_duration: f64,
}

/// Find every instance of the cut phrase in `words` (09 §2). One [`Cut`] of kind
/// [`CutKind::CutWord`] per match, spanning exactly the matched words, labeled with the
/// verbatim ASR text (so a fuzzy row shows what whisper actually heard).
///
/// `accepted` is true only for exact, confident, VAD-overlapping matches; fuzzy /
/// low-confidence / inside-silence matches arrive unaccepted for the review UI. Blank
/// phrase ⇒ no matches. Deterministic.
pub fn detect_cut_words(words: &[Word], silence: &SilenceInput, opts: &SplitOptions) -> Vec<Cut> {
    let tokens = opts.tokens();
    if tokens.is_empty() {
        return Vec::new();
    }

    let norm: Vec<String> = words.iter().map(|w| normalize(&w.text)).collect();
    let mut cuts = Vec::new();
    let mut i = 0;
    while i < words.len() {
        match match_phrase_at(&norm, words, i, &tokens, opts.fuzzy) {
            Some((end_idx, exact)) => {
                let start = words[i].start;
                let end = words[end_idx].end;
                let confident = words[i..=end_idx]
                    .iter()
                    .all(|w| w.confidence >= opts.min_confidence);
                // Hallucination guard (09 §2): a real spoken word overlaps speech; a match
                // lying entirely inside one VAD-negative run is whisper inventing words in
                // the gap — exactly where cut words live next to.
                let inside_silence = silence
                    .runs
                    .iter()
                    .any(|r| r.start <= start && end <= r.end);
                // Acceptance (09 §2, refined by measurement): an *isolated* match — one
                // standing alone between silence runs, the cut-word acoustic signature —
                // is trusted regardless of ASR confidence or spelling: whisper is
                // *expected* to be unsure about a deliberately-unusual word ("Comquad"
                // for a spoken "kumquat" arrived at 0.4 confidence). Confidence only
                // gates *embedded* exact matches; embedded non-exact matches are always
                // review-only rows.
                let trusted = is_isolated(silence, start, end) || (exact && confident);
                let label = words[i..=end_idx]
                    .iter()
                    .map(|w| w.text.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                cuts.push(Cut {
                    start,
                    end,
                    kind: CutKind::CutWord,
                    label,
                    accepted: trusted && !inside_silence,
                });
                i = end_idx + 1;
            }
            None => i += 1,
        }
    }
    cuts
}

/// Does a silence run end just before `[start, end]` and another begin just after it?
/// (The word stands alone — the cut-word acoustic signature.)
fn is_isolated(silence: &SilenceInput, start: f64, end: f64) -> bool {
    let before = silence
        .runs
        .iter()
        .any(|r| r.end >= start - ISOLATION_WINDOW && r.end <= end);
    let after = silence
        .runs
        .iter()
        .any(|r| r.start >= start && r.start <= end + ISOLATION_WINDOW);
    before && after
}

/// Does the phrase match at word `i`? Returns the last matched index and whether every
/// token matched exactly (vs. at least one fuzzy/phonetic hit). A single-token phrase also
/// tries the merged pair `words[i] + words[i+1]` (gap ≤ [`MERGE_TOKEN_GAP`]) — whisper
/// splits unfamiliar words in two ("kum quat").
fn match_phrase_at(
    norm: &[String],
    words: &[Word],
    i: usize,
    tokens: &[String],
    fuzzy: bool,
) -> Option<(usize, bool)> {
    let mut exact = true;
    'tokens: for (k, token) in tokens.iter().enumerate() {
        let idx = i + k;
        let got = norm.get(idx)?;
        if k > 0 && words[idx].start - words[idx - 1].end > MAX_PHRASE_GAP {
            return None;
        }
        if got == token {
            continue;
        }
        if fuzzy
            && token.len() >= MIN_FUZZY_LEN
            && (within_edit_one(got, token) || phonetic_key(got) == phonetic_key(token))
        {
            exact = false;
            continue 'tokens;
        }
        // Single-token phrase: try the merged neighbor pair before giving up. Both halves
        // must be real fragments (≥ 2 chars) — otherwise "a" + "kumquat" joins to an
        // edit-distance-1 "akumquat" and swallows the neighboring word.
        if tokens.len() == 1 && fuzzy && token.len() >= MIN_FUZZY_LEN && got.chars().count() >= 2 {
            if let Some(next) = norm.get(idx + 1).filter(|n| n.chars().count() >= 2) {
                if words[idx + 1].start - words[idx].end <= MERGE_TOKEN_GAP {
                    let joined = format!("{got}{next}");
                    if joined == *token
                        || within_edit_one(&joined, token)
                        || phonetic_key(&joined) == phonetic_key(token)
                    {
                        // Merged match spans two words; exact only on literal equality.
                        return Some((idx + 1, joined == *token));
                    }
                }
            }
        }
        return None;
    }
    Some((i + tokens.len() - 1, exact))
}

/// A crude consonant-skeleton phonetic key: both "kumquat" and whisper's "Comquad" fold to
/// `kmkt`, so a mangled-but-recognizable cut word still matches. Deliberately simple (not
/// full Metaphone) — both sides of every comparison pass through the same fold, so only
/// *consistency* matters, and the ≥ 5-char gate plus the isolation check bound false
/// positives.
fn phonetic_key(token: &str) -> String {
    // The one digraph worth special-casing: `ph` sounds like `f` ("phlamingo").
    let lowered = token.to_ascii_lowercase().replace("ph", "f");
    let mut key = String::with_capacity(lowered.len());
    let mut prev = '\0';
    for (i, c) in lowered
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .enumerate()
    {
        let folded = match c {
            'c' | 'q' | 'g' | 'x' => 'k',
            'd' => 't',
            'b' => 'p',
            'v' | 'f' => 'f',
            'z' | 's' => 's',
            'j' | 'y' => 'y',
            'h' | 'w' => continue,
            v @ ('a' | 'e' | 'i' | 'o' | 'u') => {
                if i == 0 {
                    // A leading vowel is identity-bearing ("echo" vs "cho"); interior
                    // vowels are what ASR mangles most, so they drop.
                    v
                } else {
                    continue;
                }
            }
            other => other,
        };
        if folded != prev {
            key.push(folded);
            prev = folded;
        }
    }
    key
}

/// Levenshtein distance ≤ 1, without building the DP table: equal, or one
/// substitution/insertion/deletion apart.
fn within_edit_one(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long): (Vec<char>, Vec<char>) = if a.chars().count() <= b.chars().count() {
        (a.chars().collect(), b.chars().collect())
    } else {
        (b.chars().collect(), a.chars().collect())
    };
    match long.len() - short.len() {
        0 => {
            // Same length: exactly one substitution allowed.
            short
                .iter()
                .zip(long.iter())
                .filter(|(x, y)| x != y)
                .count()
                == 1
        }
        1 => {
            // One insertion: walk both, allow a single skip on the long side.
            let (mut s, mut l, mut skipped) = (0usize, 0usize, false);
            while s < short.len() && l < long.len() {
                if short[s] == long[l] {
                    s += 1;
                    l += 1;
                } else if skipped {
                    return false;
                } else {
                    skipped = true;
                    l += 1;
                }
            }
            true
        }
        _ => false,
    }
}

/// Partition the timeline at every accepted [`CutKind::CutWord`] cut in `plan` (09 §3).
///
/// Each boundary's removal region is the matched words extended across abutting/overlapping
/// silence runs, backed off by the speech pads (clamped so the word span itself is always
/// removed). Overlapping removals merge. Accepted non-`CutWord` cuts interior to a part
/// carry into its EDL; ones overlapping a removal are dropped. Parts with less than
/// [`SplitOptions::min_part_secs`] of kept audio are dropped. `words` only feeds
/// [`Part::title_words`].
pub fn partition(
    plan: &CutPlan,
    words: &[Word],
    silence: &SilenceInput,
    opts: &SplitOptions,
) -> SegmentPlan {
    let duration = plan.source_duration.max(0.0);

    // Boundary removal regions, extended + padded, clamped to the timeline.
    let mut removals: Vec<TimeRange> = plan
        .cuts
        .iter()
        .filter(|c| c.accepted && c.kind == CutKind::CutWord)
        .map(|c| boundary_removal(c, silence, opts, duration))
        .filter(|r| r.duration() > 0.0)
        .collect();
    removals.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut merged: Vec<TimeRange> = Vec::with_capacity(removals.len());
    for r in removals {
        match merged.last_mut() {
            Some(prev) if r.start <= prev.end => prev.end = prev.end.max(r.end),
            _ => merged.push(r),
        }
    }

    // Interior cuts that survive into part EDLs: accepted, not boundaries, not touching one.
    let interior: Vec<TimeRange> = plan
        .cuts
        .iter()
        .filter(|c| c.accepted && c.kind != CutKind::CutWord)
        .map(|c| TimeRange::new(c.start.clamp(0.0, duration), c.end.clamp(0.0, duration)))
        .filter(|r| r.duration() > 0.0 && !merged.iter().any(|m| m.overlaps(r)))
        .collect();

    // Spans between removals → candidate parts.
    let mut spans: Vec<TimeRange> = Vec::with_capacity(merged.len() + 1);
    let mut cursor = 0.0_f64;
    for r in &merged {
        if r.start > cursor {
            spans.push(TimeRange::new(cursor, r.start));
        }
        cursor = r.end;
    }
    if cursor < duration {
        spans.push(TimeRange::new(cursor, duration));
    }

    let mut parts: Vec<Part> = Vec::with_capacity(spans.len());
    for span in spans {
        let edl = part_edl(span, &interior);
        if edl.total_duration() < opts.min_part_secs {
            continue;
        }
        let title_words: Vec<String> = words
            .iter()
            .filter(|w| w.start >= span.start && w.end <= span.end)
            .map(|w| normalize(&w.text))
            .filter(|t| !t.is_empty())
            .take(TITLE_WORDS)
            .collect();
        parts.push(Part {
            index: parts.len(),
            start: span.start,
            end: span.end,
            title_words,
            edl,
        });
    }

    SegmentPlan {
        parts,
        source_duration: duration,
    }
}

/// One boundary's removal region (09 §3): the matched words, extended over every silence
/// run that overlaps-or-touches it (to a fixed point — the run on one side can expose a
/// run on the other), then backed off by the speech pads. The pads only ever eat into
/// detected silence: the region never shrinks below the word span itself.
fn boundary_removal(
    cut: &Cut,
    silence: &SilenceInput,
    opts: &SplitOptions,
    duration: f64,
) -> TimeRange {
    // TOUCH_SECS is a tolerance, so boundary equality must not hinge on f64 exactness
    // (5.6 + 0.05 lands one ulp under 5.65 and would silently skip the trailing run).
    const EPS: f64 = 1e-9;
    let word = TimeRange::new(cut.start.clamp(0.0, duration), cut.end.clamp(0.0, duration));
    let mut region = word;
    loop {
        let before = region;
        for run in &silence.runs {
            if run.start <= region.end + TOUCH_SECS + EPS
                && region.start <= run.end + TOUCH_SECS + EPS
            {
                region.start = region.start.min(run.start);
                region.end = region.end.max(run.end);
            }
        }
        if region == before {
            break;
        }
    }
    region.start = region.start.max(0.0);
    region.end = region.end.min(duration);
    TimeRange::new(
        (region.start + opts.speech_pad_post).min(word.start),
        (region.end - opts.speech_pad_pre).max(word.end),
    )
}

/// The single-source EDL for one part span: kept ranges inside `[span.start, span.end]`
/// minus the interior cuts (clamped to the span). Mirrors [`crate::to_edl`]'s alternating
/// kept/cut walk, bounded to the span.
fn part_edl(span: TimeRange, interior: &[TimeRange]) -> Edl {
    let mut inside: Vec<TimeRange> = interior
        .iter()
        .map(|r| {
            TimeRange::new(
                r.start.clamp(span.start, span.end),
                r.end.clamp(span.start, span.end),
            )
        })
        .filter(|r| r.duration() > 0.0)
        .collect();
    inside.sort_by(|a, b| a.start.total_cmp(&b.start));

    let mut segments: Vec<Segment> = Vec::new();
    let mut cursor = span.start;
    for cut in &inside {
        if cut.start > cursor {
            segments.push(Segment::kept(0, cursor, cut.start));
        }
        segments.push(Segment::cut(0, cut.start, cut.end));
        cursor = cursor.max(cut.end);
    }
    if cursor < span.end {
        segments.push(Segment::kept(0, cursor, span.end));
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

    /// Two 10-word "scripts" with the cut word between: script A at 0.5–4.0 s, "kumquat" at
    /// 5.0–5.6 s, script B at 7.0–10.5 s. Silence runs cover the gaps.
    fn teleprompter_fixture() -> (Vec<Word>, SilenceInput) {
        let mut words = vec![
            word("Why", 0.5, 0.8, 0.95),
            word("nobody", 0.9, 1.3, 0.95),
            word("holds", 1.4, 1.8, 0.95),
            word("the", 1.9, 2.0, 0.95),
            word("door.", 2.1, 4.0, 0.95),
            word("Kumquat.", 5.0, 5.6, 0.92),
            word("Second", 7.0, 7.4, 0.95),
            word("script", 7.5, 7.9, 0.95),
            word("starts", 8.0, 8.4, 0.95),
            word("here.", 8.5, 10.5, 0.95),
        ];
        words.sort_by(|a, b| a.start.total_cmp(&b.start));
        let silence = SilenceInput::from_runs([(4.0, 4.95), (5.65, 7.0)]);
        (words, silence)
    }

    fn plan_from(cuts: Vec<Cut>, duration: f64) -> CutPlan {
        CutPlan {
            cuts,
            source_duration: duration,
        }
    }

    /// 09 §8 golden: one cut word → two parts, boundaries at silence edges backed off by
    /// the speech pads, word gone from both.
    #[test]
    fn golden_two_parts_with_padded_boundaries() {
        let (words, silence) = teleprompter_fixture();
        let opts = SplitOptions::for_phrase("kumquat");
        let cuts = detect_cut_words(&words, &silence, &opts);
        assert_eq!(cuts.len(), 1);
        assert!(cuts[0].accepted);
        assert_eq!(cuts[0].kind, CutKind::CutWord);
        assert_eq!(cuts[0].label, "Kumquat.");

        let plan = plan_from(cuts, 11.0);
        let sp = partition(&plan, &words, &silence, &opts);
        assert_eq!(sp.parts.len(), 2);

        // Removal region: word 5.0–5.6 extended over runs (4.0–4.95 touches within 0.05;
        // 5.65–7.0 touches) → 4.0–7.0; pads: start+0.30 = 4.3, end−0.15 = 6.85.
        let a = &sp.parts[0];
        let b = &sp.parts[1];
        assert!((a.start - 0.0).abs() < 1e-9);
        assert!((a.end - 4.3).abs() < 1e-9, "part A end {}", a.end);
        assert!((b.start - 6.85).abs() < 1e-9, "part B start {}", b.start);
        assert!((b.end - 11.0).abs() < 1e-9);
        assert_eq!(a.index, 0);
        assert_eq!(b.index, 1);

        // The cut word's span is inside neither part.
        for p in &sp.parts {
            assert!(p.end <= 5.0 || p.start >= 5.6);
        }

        // Naming: first words of each script.
        assert_eq!(a.title_words, vec!["why", "nobody", "holds", "the"]);
        assert_eq!(b.title(), "second script starts here");
    }

    /// The pads never invert a tight boundary: with no surrounding silence the removal is
    /// exactly the word span.
    #[test]
    fn pads_clamp_to_word_span_without_silence() {
        let words = vec![
            word("a", 0.0, 1.0, 0.9),
            word("kumquat", 1.0, 1.5, 0.9),
            word("b", 1.5, 2.5, 0.9),
        ];
        let silence = SilenceInput::default();
        let opts = SplitOptions {
            min_part_secs: 0.2,
            ..SplitOptions::for_phrase("kumquat")
        };
        let cuts = detect_cut_words(&words, &silence, &opts);
        let sp = partition(&plan_from(cuts, 2.5), &words, &silence, &opts);
        assert_eq!(sp.parts.len(), 2);
        assert!((sp.parts[0].end - 1.0).abs() < 1e-9);
        assert!((sp.parts[1].start - 1.5).abs() < 1e-9);
    }

    /// Fuzzy match ("cumquat") is proposed but unaccepted (09 §2), and `--exact` disables it.
    #[test]
    fn fuzzy_matches_arrive_unaccepted() {
        let words = vec![word("Cumquat,", 1.0, 1.5, 0.95)];
        let silence = SilenceInput::default();
        let opts = SplitOptions::for_phrase("kumquat");
        let cuts = detect_cut_words(&words, &silence, &opts);
        assert_eq!(cuts.len(), 1);
        assert!(!cuts[0].accepted, "fuzzy must be a possible-match row");
        assert_eq!(cuts[0].label, "Cumquat,");

        let exact_only = SplitOptions {
            fuzzy: false,
            ..opts
        };
        assert!(detect_cut_words(&words, &silence, &exact_only).is_empty());
    }

    /// The real-world whisper mangle: a spoken "kumquat" transcribed as "Comquad" (edit
    /// distance 3) still matches via the phonetic key, and because it stands alone between
    /// silence runs it is trusted (accepted) — the isolation signature of a cut word.
    #[test]
    fn phonetic_match_isolated_between_silences_is_accepted() {
        let words = vec![
            word("door.", 2.0, 4.0, 0.95),
            word("Comquad", 5.0, 5.6, 0.9),
            word("Three", 7.0, 7.4, 0.95),
        ];
        let silence = SilenceInput::from_runs([(4.0, 4.95), (5.65, 7.0)]);
        let cuts = detect_cut_words(&words, &silence, &SplitOptions::for_phrase("kumquat"));
        assert_eq!(cuts.len(), 1);
        assert!(cuts[0].accepted, "isolated phonetic match must be trusted");
        assert_eq!(cuts[0].label, "Comquad");

        // The same phonetic match embedded in flowing speech (no isolation) stays a
        // review-only row.
        let embedded = vec![
            word("the", 1.0, 1.2, 0.95),
            word("Comquad", 1.3, 1.9, 0.9),
            word("thing", 2.0, 2.4, 0.95),
        ];
        let cuts = detect_cut_words(
            &embedded,
            &SilenceInput::default(),
            &SplitOptions::for_phrase("kumquat"),
        );
        assert_eq!(cuts.len(), 1);
        assert!(!cuts[0].accepted);
    }

    /// Whisper splitting the word in two ("kum quat", tight gap) merges into one match.
    #[test]
    fn merged_token_pair_matches_single_token_phrase() {
        let words = vec![word("kum", 5.0, 5.3, 0.9), word("quat", 5.35, 5.6, 0.9)];
        let silence = SilenceInput::from_runs([(4.0, 4.95), (5.65, 7.0)]);
        let cuts = detect_cut_words(&words, &silence, &SplitOptions::for_phrase("kumquat"));
        assert_eq!(cuts.len(), 1);
        assert!((cuts[0].start - 5.0).abs() < 1e-9);
        assert!((cuts[0].end - 5.6).abs() < 1e-9);
        assert_eq!(cuts[0].label, "kum quat");
        assert!(cuts[0].accepted, "isolated merged match must be trusted");

        // A wide gap between the halves does not merge.
        let apart = vec![word("kum", 5.0, 5.3, 0.9), word("quat", 5.8, 6.1, 0.9)];
        assert!(
            detect_cut_words(&apart, &silence, &SplitOptions::for_phrase("kumquat")).is_empty()
        );
    }

    /// The phonetic fold itself.
    #[test]
    fn phonetic_key_folds_consistently() {
        assert_eq!(phonetic_key("kumquat"), phonetic_key("Comquad"));
        assert_eq!(phonetic_key("kumquat"), "kmkt");
        assert_eq!(phonetic_key("flamingo"), phonetic_key("phlamingo"));
        assert_ne!(phonetic_key("kumquat"), phonetic_key("compact"));
        assert_ne!(phonetic_key("kumquat"), phonetic_key("comment"));
        // Leading vowels are identity-bearing.
        assert_ne!(phonetic_key("echo"), phonetic_key("cho"));
    }

    /// Short tokens never fuzzy-match (edit distance 1 on "cat"/"cut" would be carnage).
    #[test]
    fn short_tokens_do_not_fuzzy_match() {
        let words = vec![word("cat", 1.0, 1.3, 0.95)];
        let opts = SplitOptions::for_phrase("cut");
        let cuts = detect_cut_words(&words, &SilenceInput::default(), &opts);
        assert!(cuts.is_empty());
    }

    /// Embedded low-confidence and inside-silence matches are flagged, not auto-accepted;
    /// an *isolated* low-confidence match IS accepted — whisper is expected to be unsure
    /// about a deliberately-unusual word (09 §2, measured on the TTS fixture).
    #[test]
    fn low_confidence_and_hallucination_guard() {
        let silence = SilenceInput::from_runs([(4.0, 8.0)]);
        // Low confidence, embedded in speech (no isolation) → review-only.
        let low = vec![word("kumquat", 1.0, 1.5, 0.3)];
        let cuts = detect_cut_words(&low, &silence, &SplitOptions::for_phrase("kumquat"));
        assert_eq!(cuts.len(), 1);
        assert!(!cuts[0].accepted);
        // Confident but entirely inside a VAD-negative run → hallucination suspect.
        let ghost = vec![word("kumquat", 5.0, 5.5, 0.95)];
        let cuts = detect_cut_words(&ghost, &silence, &SplitOptions::for_phrase("kumquat"));
        assert_eq!(cuts.len(), 1);
        assert!(!cuts[0].accepted);
        // Low confidence but isolated between silence runs → trusted.
        let isolated_low = vec![word("kumquat", 8.2, 8.7, 0.3)];
        let silence = SilenceInput::from_runs([(4.0, 8.0), (8.8, 10.0)]);
        let cuts = detect_cut_words(
            &isolated_low,
            &silence,
            &SplitOptions::for_phrase("kumquat"),
        );
        assert_eq!(cuts.len(), 1);
        assert!(cuts[0].accepted, "isolation outranks confidence");
    }

    /// Multi-word phrases match consecutively and respect the inter-word gap rule.
    #[test]
    fn multi_word_phrase_with_gap_rule() {
        let opts = SplitOptions::for_phrase("next script");
        let silence = SilenceInput::default();
        let tight = vec![
            word("Next", 1.0, 1.3, 0.95),
            word("script.", 1.4, 1.8, 0.95),
        ];
        let cuts = detect_cut_words(&tight, &silence, &opts);
        assert_eq!(cuts.len(), 1);
        assert!(cuts[0].accepted);
        assert!((cuts[0].start - 1.0).abs() < 1e-9);
        assert!((cuts[0].end - 1.8).abs() < 1e-9);

        // A 0.9 s pause between the words breaks the phrase.
        let split_apart = vec![
            word("Next", 1.0, 1.3, 0.95),
            word("script.", 2.2, 2.6, 0.95),
        ];
        assert!(detect_cut_words(&split_apart, &silence, &opts).is_empty());
    }

    /// The merged-pair fallback never swallows a real neighboring word: "a kumquat" in
    /// flowing speech matches only the "kumquat" token itself, not the joined pair.
    #[test]
    fn merge_never_swallows_a_neighboring_word() {
        let words = vec![word("a", 0.0, 1.0, 0.9), word("kumquat", 1.0, 1.5, 0.9)];
        let cuts = detect_cut_words(
            &words,
            &SilenceInput::default(),
            &SplitOptions::for_phrase("kumquat"),
        );
        assert_eq!(cuts.len(), 1);
        assert!(
            (cuts[0].start - 1.0).abs() < 1e-9,
            "must not include the 'a'"
        );
        assert_eq!(cuts[0].label, "kumquat");
    }

    /// Doubled cut word → one merged boundary, no empty middle part; cut word at the file
    /// edge → degenerate leading/trailing part dropped.
    #[test]
    fn doubled_and_edge_cut_words_drop_degenerate_parts() {
        let words = vec![
            word("kumquat", 0.2, 0.7, 0.95), // leading — part before it is < min
            word("script", 2.0, 5.0, 0.95),
            word("kumquat", 6.0, 6.5, 0.95),
            word("kumquat", 6.8, 7.3, 0.95), // said twice for safety
            word("closing", 9.0, 11.0, 0.95),
        ];
        let silence = SilenceInput::from_runs([
            (0.0, 0.15),
            (0.75, 1.95),
            (5.05, 5.95),
            (6.55, 6.75),
            (7.35, 8.95),
        ]);
        let opts = SplitOptions::for_phrase("kumquat");
        let cuts = detect_cut_words(&words, &silence, &opts);
        assert_eq!(cuts.len(), 3);
        assert!(cuts.iter().all(|c| c.accepted));

        let sp = partition(&plan_from(cuts, 11.5), &words, &silence, &opts);
        assert_eq!(sp.parts.len(), 2, "parts: {:?}", sp.parts);
        assert_eq!(sp.parts[0].title_words, vec!["script"]);
        assert_eq!(sp.parts[1].title_words, vec!["closing"]);
    }

    /// Interior filler cuts survive into the owning part's EDL; ones overlapping a
    /// boundary removal are dropped (09 §3).
    #[test]
    fn interior_cuts_carry_into_parts() {
        let (words, silence) = teleprompter_fixture();
        let opts = SplitOptions::for_phrase("kumquat");
        let mut cuts = detect_cut_words(&words, &silence, &opts);
        // An accepted filler inside part A…
        cuts.push(Cut {
            start: 1.0,
            end: 1.2,
            kind: CutKind::Filler,
            label: "um".into(),
            accepted: true,
        });
        // …and one overlapping the boundary removal (4.0–7.0 region) — must be dropped.
        cuts.push(Cut {
            start: 6.0,
            end: 6.2,
            kind: CutKind::Silence,
            label: "silence".into(),
            accepted: true,
        });
        let sp = partition(&plan_from(cuts, 11.0), &words, &silence, &opts);
        assert_eq!(sp.parts.len(), 2);
        let a = &sp.parts[0];
        // Part A: kept 0–1.0, cut 1.0–1.2, kept 1.2–4.3.
        assert_eq!(a.edl.kept_ranges().count(), 2);
        assert!((a.kept_secs() - (4.3 - 0.2)).abs() < 1e-9);
        // Part B is one clean kept span (the overlapping silence cut was dropped).
        assert_eq!(sp.parts[1].edl.kept_ranges().count(), 1);
    }

    /// Rejected cut words are not boundaries: everything stays one part.
    #[test]
    fn rejected_cut_word_does_not_split() {
        let (words, silence) = teleprompter_fixture();
        let opts = SplitOptions::for_phrase("kumquat");
        let mut cuts = detect_cut_words(&words, &silence, &opts);
        cuts[0].accepted = false;
        let sp = partition(&plan_from(cuts, 11.0), &words, &silence, &opts);
        assert_eq!(sp.parts.len(), 1);
        assert!((sp.parts[0].kept_secs() - 11.0).abs() < 1e-9);
    }

    /// Determinism (06 §2): identical inputs ⇒ identical matches and plan.
    #[test]
    fn detection_and_partition_are_deterministic() {
        let (words, silence) = teleprompter_fixture();
        let opts = SplitOptions::for_phrase("kumquat");
        let c1 = detect_cut_words(&words, &silence, &opts);
        let c2 = detect_cut_words(&words, &silence, &opts);
        assert_eq!(c1, c2);
        let p1 = partition(&plan_from(c1.clone(), 11.0), &words, &silence, &opts);
        let p2 = partition(&plan_from(c2, 11.0), &words, &silence, &opts);
        assert_eq!(p1, p2);
    }

    /// The wire contract: `CutKind::CutWord` serializes as `"cut_word"`, and pre-split
    /// JSON (silence/filler only) still deserializes.
    #[test]
    fn cut_word_kind_round_trips_and_stays_additive() {
        let json = serde_json::to_string(&CutKind::CutWord).unwrap();
        assert_eq!(json, "\"cut_word\"");
        let back: CutKind = serde_json::from_str("\"cut_word\"").unwrap();
        assert_eq!(back, CutKind::CutWord);
        assert_eq!(CutKind::CutWord.as_str(), "cut_word");
        let legacy: Vec<CutKind> = serde_json::from_str(r#"["silence","filler"]"#).unwrap();
        assert_eq!(legacy, vec![CutKind::Silence, CutKind::Filler]);
    }

    /// `within_edit_one` sanity across the three edit shapes.
    #[test]
    fn edit_distance_one_shapes() {
        assert!(within_edit_one("kumquat", "kumquat"));
        assert!(within_edit_one("cumquat", "kumquat")); // substitution
        assert!(within_edit_one("kumqat", "kumquat")); // deletion
        assert!(within_edit_one("kumquaat", "kumquat")); // insertion
        assert!(!within_edit_one("comcast", "kumquat"));
        assert!(!within_edit_one("kum", "kumquat"));
    }
}
