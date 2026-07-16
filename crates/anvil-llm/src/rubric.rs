//! The eval rubric: "would a podcaster publish this without a major edit?"
//!
//! 05-MILESTONES sets the M4 exit gate for this lane: *"shownotes rated usable-without-major-
//! edit ≥80% on a 20-episode rubric"*. That final judgment is a human one — a person reads the
//! notes and says yes or no. But most of the ways a local 4-bit model fails are **structural**
//! and a machine can catch them for free, on every run, in CI:
//!
//! - it wrote three chapters and put two of them at the same second;
//! - it invented a chapter at 1:04:00 of a 40-minute episode;
//! - the summary is one word, or an essay;
//! - it returned two bullets, or twelve;
//! - it forgot the tags entirely.
//!
//! Every one of those means "the user has to fix this before publishing" — i.e. **not usable
//! without a major edit** — regardless of how good the prose is. So this module scores the
//! structure, and the human rubric below scores the substance. A build that fails
//! [`RubricReport::usable`] never reaches a human panel.
//!
//! ## The machine checks
//! Each [`Check`] carries a weight; [`RubricReport::score`] is the weighted fraction passed.
//! Some checks are **required**: fail one and the notes are unusable whatever the score
//! (a chapter list that runs backwards is not "80% good", it is broken).
//!
//! ## The human rubric (06-QUALITY-EVAL, run per episode on the 20-episode set)
//! Scored 1–5, "usable without a major edit" = **every** row ≥ 3:
//! 1. **Accuracy** — does it claim anything that was not said? (a 1 here fails the episode
//!    outright, however good the rest is: an invented sponsor is not a small edit)
//! 2. **Summary** — would this make someone press play, and is it about *this* episode?
//! 3. **Chapters** — do the markers land where the topic actually changes, and are the titles
//!    what a listener would scan for?
//! 4. **Titles** — is at least one usable as-is?
//! 5. **Tags** — are they the words a listener would search?
//!
//! Record the [`crate::prompt::PROMPT_VERSION`] and the model pack id with every score sheet;
//! a rubric number without them is not attributable to anything.

use crate::{Shownotes, TranscriptInput};

/// Weighted fraction at or above which the structure is considered publishable.
pub const USABLE_THRESHOLD: f32 = 0.8;

/// What the checks are measured against.
#[derive(Debug, Clone)]
pub struct RubricContext {
    /// Episode duration in seconds — chapters must live inside it.
    pub duration_secs: f64,
    /// The earliest legal chapter start (the transcript's first segment).
    pub first_start_secs: f64,
    /// Minimum bullets before the notes read as thin.
    pub min_bullets: usize,
    /// Minimum keywords/tags.
    pub min_keywords: usize,
    /// Minimum chapters for an episode longer than [`Self::chapters_expected_after_secs`].
    pub min_chapters: usize,
    /// Below this duration a single chapter is fine.
    pub chapters_expected_after_secs: f64,
}

impl Default for RubricContext {
    fn default() -> Self {
        Self {
            duration_secs: f64::INFINITY,
            first_start_secs: 0.0,
            min_bullets: 3,
            min_keywords: 3,
            min_chapters: 2,
            chapters_expected_after_secs: 300.0,
        }
    }
}

impl RubricContext {
    /// The rubric context implied by a transcript.
    pub fn from_input(input: &TranscriptInput) -> Self {
        Self {
            duration_secs: input.duration_secs(),
            first_start_secs: input.segments.first().map(|s| s.start).unwrap_or(0.0),
            ..Default::default()
        }
    }
}

/// One structural check.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    /// Stable id, for CSV/report columns: `chapters_monotonic`, `summary_length`, …
    pub id: &'static str,
    /// Did it pass?
    pub passed: bool,
    /// A failure a human must be able to act on without reading this source file.
    pub detail: String,
    /// Contribution to [`RubricReport::score`].
    pub weight: f32,
    /// A failed required check makes the notes unusable regardless of the score.
    pub required: bool,
}

/// The outcome of scoring one episode's show notes.
#[derive(Debug, Clone, PartialEq)]
pub struct RubricReport {
    /// Every check, in a stable order.
    pub checks: Vec<Check>,
    /// Weighted fraction of checks passed, in `[0, 1]`.
    pub score: f32,
    /// Structurally publishable: all required checks passed and `score >=`
    /// [`USABLE_THRESHOLD`].
    pub usable: bool,
}

impl RubricReport {
    /// Ids of the checks that failed — the one-line summary a CI log wants.
    pub fn failures(&self) -> Vec<&'static str> {
        self.checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.id)
            .collect()
    }
}

/// Score show notes against the structural rubric.
pub fn score(notes: &Shownotes, ctx: &RubricContext) -> RubricReport {
    let mut checks = Vec::new();
    let mut check = |id, passed, detail: String, weight, required| {
        checks.push(Check {
            id,
            passed,
            detail,
            weight,
            required,
        })
    };

    // --- summary
    let summary_words = notes.summary.split_whitespace().count();
    check(
        "summary_length",
        (20..=250).contains(&summary_words),
        format!("summary is {summary_words} words, want 20-250"),
        2.0,
        true,
    );

    // --- bullets
    let bullets_ok = notes.bullets.len() >= ctx.min_bullets
        && notes.bullets.len() <= 10
        && notes.bullets.iter().all(|b| !b.trim().is_empty());
    check(
        "bullets_count",
        bullets_ok,
        format!(
            "{} bullets, want {}-10 and none empty",
            notes.bullets.len(),
            ctx.min_bullets
        ),
        1.5,
        true,
    );

    // --- chapters
    let want_chapters = if ctx.duration_secs > ctx.chapters_expected_after_secs {
        ctx.min_chapters
    } else {
        1
    };
    check(
        "chapters_count",
        notes.chapters.len() >= want_chapters,
        format!(
            "{} chapters, want at least {want_chapters} for a {:.0} s episode",
            notes.chapters.len(),
            ctx.duration_secs
        ),
        1.5,
        false,
    );

    let monotonic = notes
        .chapters
        .windows(2)
        .all(|w| w[1].start > w[0].start && w[0].start.is_finite());
    check(
        "chapters_monotonic",
        monotonic,
        "chapter starts must strictly increase".into(),
        2.0,
        true,
    );

    let in_range = notes
        .chapters
        .iter()
        .all(|c| c.start >= ctx.first_start_secs && c.start <= ctx.duration_secs);
    check(
        "chapters_in_range",
        in_range,
        format!(
            "every chapter must start within [{:.1}, {:.1}] s",
            ctx.first_start_secs, ctx.duration_secs
        ),
        2.0,
        true,
    );

    let opens_episode = notes
        .chapters
        .first()
        .is_none_or(|c| (c.start - ctx.first_start_secs).abs() < 1.0);
    check(
        "chapters_open_the_episode",
        opens_episode,
        "the first chapter must start at the top of the episode".into(),
        1.0,
        false,
    );

    let titled = notes
        .chapters
        .iter()
        .all(|c| !c.title.trim().is_empty() && c.title.chars().count() <= 80);
    check(
        "chapters_titled",
        titled,
        "every chapter needs a title of at most 80 characters".into(),
        1.0,
        true,
    );

    // --- titles
    let titles_ok = !notes.titles.is_empty()
        && notes
            .titles
            .iter()
            .all(|t| !t.trim().is_empty() && t.chars().count() <= 120);
    check(
        "titles_present",
        titles_ok,
        format!(
            "{} title options, want at least 1, each under 120 characters",
            notes.titles.len()
        ),
        1.0,
        true,
    );

    // --- keywords
    check(
        "keywords_present",
        notes.keywords.len() >= ctx.min_keywords
            && notes.keywords.iter().all(|k| !k.trim().is_empty()),
        format!(
            "{} keywords, want at least {}",
            notes.keywords.len(),
            ctx.min_keywords
        ),
        1.0,
        false,
    );

    let total: f32 = checks.iter().map(|c| c.weight).sum();
    let earned: f32 = checks.iter().filter(|c| c.passed).map(|c| c.weight).sum();
    let score = if total > 0.0 { earned / total } else { 0.0 };
    let required_ok = checks.iter().all(|c| !c.required || c.passed);

    RubricReport {
        checks,
        score,
        usable: required_ok && score >= USABLE_THRESHOLD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Chapter;

    fn good() -> Shownotes {
        Shownotes {
            summary: "Two audio engineers talk through recording a podcast in a room that fights \
                      back. They cover why a dynamic microphone forgives an untreated space, what \
                      to hang on the wall first, and how to cut filler words without making the \
                      conversation sound clipped. It ends with a short argument about editing \
                      software that nobody wins."
                .into(),
            bullets: vec![
                "A dynamic microphone rejects far more of the room than a condenser.".into(),
                "Treat the wall behind the speaker before buying anything else.".into(),
                "Cutting filler words from the transcript view is faster than the timeline.".into(),
            ],
            chapters: vec![
                Chapter {
                    title: "Cold open".into(),
                    start: 0.0,
                },
                Chapter {
                    title: "Microphones".into(),
                    start: 300.0,
                },
                Chapter {
                    title: "Room treatment".into(),
                    start: 1200.0,
                },
            ],
            titles: vec!["Recording in a room that fights back".into()],
            keywords: vec!["microphones".into(), "acoustics".into(), "editing".into()],
        }
    }

    fn ctx() -> RubricContext {
        RubricContext {
            duration_secs: 1_800.0,
            ..Default::default()
        }
    }

    #[test]
    fn well_formed_shownotes_are_usable() {
        let report = score(&good(), &ctx());
        assert!(report.usable, "{report:#?}");
        assert_eq!(report.score, 1.0);
        assert!(report.failures().is_empty());
    }

    #[test]
    fn chapters_that_run_backwards_are_never_usable_however_good_the_prose() {
        let mut notes = good();
        notes.chapters[2].start = 120.0; // now 0, 300, 120
        let report = score(&notes, &ctx());
        assert!(!report.usable);
        assert!(report.failures().contains(&"chapters_monotonic"));
        // A required failure is disqualifying even though almost everything else passed.
        assert!(report.score > USABLE_THRESHOLD, "{}", report.score);
    }

    #[test]
    fn a_chapter_past_the_end_of_the_episode_fails() {
        let mut notes = good();
        notes.chapters[2].start = 5_400.0;
        let report = score(&notes, &ctx());
        assert!(!report.usable);
        assert!(report.failures().contains(&"chapters_in_range"));
    }

    #[test]
    fn a_one_word_summary_and_no_bullets_fail() {
        let notes = Shownotes {
            summary: "Audio.".into(),
            bullets: vec![],
            ..good()
        };
        let report = score(&notes, &ctx());
        assert!(!report.usable);
        assert!(report.failures().contains(&"summary_length"));
        assert!(report.failures().contains(&"bullets_count"));
        assert!(report.score < USABLE_THRESHOLD);
    }

    #[test]
    fn empty_shownotes_score_zero_and_are_not_usable() {
        let report = score(&Shownotes::default(), &RubricContext::default());
        assert!(!report.usable);
        assert!(report.score < 0.5, "{}", report.score);
    }

    #[test]
    fn a_short_episode_is_allowed_a_single_chapter() {
        let notes = Shownotes {
            chapters: vec![Chapter {
                title: "The whole thing".into(),
                start: 0.0,
            }],
            ..good()
        };
        let short = RubricContext {
            duration_secs: 240.0,
            ..Default::default()
        };
        assert!(score(&notes, &short).usable);
        // The same notes on a 30-minute episode lose the (non-required) count check.
        let long = score(&notes, &ctx());
        assert!(long.failures().contains(&"chapters_count"));
    }
}
