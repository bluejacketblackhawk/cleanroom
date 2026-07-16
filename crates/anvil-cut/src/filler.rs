//! Filler cutting (03 §5): from ASR word timestamps, match a language-specific disfluency
//! lexicon, gate on confidence, pad `±40 ms`, and emit one accept/reject [`Cut`] per instance.
//! `aggressive` mode also removes the hedges `you know` / `like`. Fillers landing inside a
//! music segment are skipped (defensive — §5 keeps cutting off music beds).

use crate::{Cut, CutKind, CutOptions, SilenceInput, Word};

/// Base English filler lexicon (03 §5: `um, uh, erm, hmm`). These are single tokens with no
/// non-filler meaning, so they are always safe to cut when confident.
const EN_BASE: &[&str] = &["um", "uh", "erm", "hmm", "uhm", "mm"];
/// Extra single-word hedges cut only in `aggressive` mode (03 §5: `like`).
const EN_AGGRESSIVE_SINGLE: &[&str] = &["like"];
/// Extra multi-word hedges cut only in `aggressive` mode (03 §5: `you know`).
const EN_AGGRESSIVE_PHRASES: &[&[&str]] = &[&["you", "know"]];

/// Emit filler [`Cut`]s for `words` (03 §5).
pub(crate) fn plan_filler_cuts(
    language: &str,
    words: &[Word],
    silence: &SilenceInput,
    opts: &CutOptions,
) -> Vec<Cut> {
    if !language_supported(language) {
        // Only en fillers are implemented in M3; de/es/ja lexicons are a later task (03 §5:
        // "language-specific lexicon"). Non-en input yields no filler cuts rather than
        // wrong-language false positives.
        return Vec::new();
    }

    // Pre-normalize every token once (lowercased, punctuation stripped).
    let norm: Vec<String> = words.iter().map(|w| normalize(&w.text)).collect();
    let mut cuts = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if let Some(span) = match_phrase(&norm, i, opts.aggressive) {
            let (end_idx, label) = span;
            // Every word in the phrase must clear the confidence gate (03 §5: "≥ 0.85").
            let confident = words[i..=end_idx]
                .iter()
                .all(|w| w.confidence >= opts.filler_min_confidence);
            if confident {
                let start = words[i].start - opts.filler_pad;
                let end = words[end_idx].end + opts.filler_pad;
                let cut = Cut {
                    start: start.max(0.0),
                    end,
                    kind: CutKind::Filler,
                    label,
                    accepted: true,
                };
                // Defensive music guard: don't cut a filler that overlaps a music bed.
                if !silence.music.iter().any(|m| m.overlaps(&cut.range())) {
                    cuts.push(cut);
                }
            }
            i = end_idx + 1;
        } else {
            i += 1;
        }
    }
    cuts
}

/// Does word `i` begin a filler? Returns the last word index it spans plus a review label.
/// Prefers the longest match (a two-word phrase over its first word).
fn match_phrase(norm: &[String], i: usize, aggressive: bool) -> Option<(usize, String)> {
    if aggressive {
        for phrase in EN_AGGRESSIVE_PHRASES {
            if matches_at(norm, i, phrase) {
                let end = i + phrase.len() - 1;
                return Some((end, phrase.join(" ")));
            }
        }
    }
    let token = norm.get(i)?;
    if EN_BASE.contains(&token.as_str())
        || (aggressive && EN_AGGRESSIVE_SINGLE.contains(&token.as_str()))
    {
        return Some((i, token.clone()));
    }
    None
}

/// Whether `norm[i..]` starts with the token sequence `phrase`.
fn matches_at(norm: &[String], i: usize, phrase: &[&str]) -> bool {
    phrase
        .iter()
        .enumerate()
        .all(|(k, &tok)| norm.get(i + k).map(String::as_str) == Some(tok))
}

/// Lowercase and strip surrounding punctuation/whitespace (ASR often attaches `,`/`.`), so
/// `"Um,"` matches `um`. Keeps internal apostrophes (e.g. contractions) intact.
fn normalize(text: &str) -> String {
    text.trim()
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
        .to_lowercase()
}

/// Only English is implemented in M3. Empty language (unknown) is treated as English, which
/// is the common ASR default.
fn language_supported(language: &str) -> bool {
    let lang = language.trim().to_lowercase();
    lang.is_empty() || lang == "en" || lang.starts_with("en-")
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

    fn labeled_fixture() -> (Vec<Word>, Vec<bool>) {
        // (word, truth = is a real filler). Non-aggressive mode is assumed.
        let rows: &[(&str, f64, f64, f32, bool)] = &[
            ("So", 0.0, 0.3, 0.95, false),
            ("um", 0.4, 0.7, 0.97, true),
            ("the", 0.8, 1.0, 0.96, false),
            ("uh", 1.1, 1.35, 0.90, true),
            ("point", 1.4, 1.8, 0.95, false),
            ("is", 1.9, 2.0, 0.94, false),
            ("erm", 2.1, 2.4, 0.88, true),
            ("like", 2.5, 2.7, 0.93, false), // real word, not a filler outside aggressive
            ("we", 2.8, 3.0, 0.95, false),
            ("hmm", 3.1, 3.5, 0.80, true), // true filler but conf < 0.85 → missed (recall hit)
            ("know", 3.6, 3.9, 0.90, false),
        ];
        let words = rows
            .iter()
            .map(|&(t, s, e, c, _)| word(t, s, e, c))
            .collect();
        let truth = rows.iter().map(|&(.., f)| f).collect();
        (words, truth)
    }

    /// Filler precision/recall on a small labeled set (06 §2 gate: P ≥ 0.90, R ≥ 0.70).
    #[test]
    fn filler_precision_recall_meets_gate() {
        let (words, truth) = labeled_fixture();
        let silence = SilenceInput::default();
        let opts = CutOptions::default();
        let cuts = plan_filler_cuts("en", &words, &silence, &opts);

        // A detected cut is a true positive if it overlaps a truth-filler word.
        let truth_ranges: Vec<(f64, f64)> = words
            .iter()
            .zip(truth.iter())
            .filter(|(_, &t)| t)
            .map(|(w, _)| (w.start, w.end))
            .collect();

        let overlaps = |a: (f64, f64), b: (f64, f64)| a.0 < b.1 && b.0 < a.1;
        let mut tp = 0usize;
        let mut fp = 0usize;
        for c in &cuts {
            if truth_ranges.iter().any(|&r| overlaps((c.start, c.end), r)) {
                tp += 1;
            } else {
                fp += 1;
            }
        }
        let matched_truth = truth_ranges
            .iter()
            .filter(|&&r| cuts.iter().any(|c| overlaps((c.start, c.end), r)))
            .count();
        let fnn = truth_ranges.len() - matched_truth;

        let precision = tp as f64 / (tp + fp).max(1) as f64;
        let recall = tp as f64 / (tp + fnn).max(1) as f64;

        // Detected: um, uh, erm (3). Not: hmm (conf 0.80), like (not aggressive).
        assert_eq!(tp, 3, "unexpected TP; cuts={cuts:?}");
        assert_eq!(fp, 0, "unexpected FP; cuts={cuts:?}");
        assert_eq!(fnn, 1, "expected the low-confidence hmm to be missed");
        assert!(precision >= 0.90, "precision {precision} < 0.90");
        assert!(recall >= 0.70, "recall {recall} < 0.70");
    }

    #[test]
    fn confidence_gate_keeps_uncertain_fillers() {
        let words = vec![word("um", 1.0, 1.3, 0.5)];
        let cuts = plan_filler_cuts(
            "en",
            &words,
            &SilenceInput::default(),
            &CutOptions::default(),
        );
        assert!(cuts.is_empty());
    }

    #[test]
    fn padding_is_applied() {
        let words = vec![word("uh", 2.0, 2.2, 0.95)];
        let cuts = plan_filler_cuts(
            "en",
            &words,
            &SilenceInput::default(),
            &CutOptions::default(),
        );
        assert_eq!(cuts.len(), 1);
        assert!((cuts[0].start - 1.96).abs() < 1e-9);
        assert!((cuts[0].end - 2.24).abs() < 1e-9);
    }

    #[test]
    fn punctuation_and_case_normalize() {
        let words = vec![word("Um,", 0.5, 0.8, 0.95)];
        let cuts = plan_filler_cuts(
            "en",
            &words,
            &SilenceInput::default(),
            &CutOptions::default(),
        );
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].label, "um");
    }

    #[test]
    fn aggressive_mode_cuts_hedges() {
        let opts = CutOptions {
            aggressive: true,
            ..CutOptions::default()
        };
        let words = vec![
            word("you", 0.0, 0.2, 0.95),
            word("know", 0.2, 0.5, 0.95),
            word("like", 1.0, 1.2, 0.95),
        ];
        let cuts = plan_filler_cuts("en", &words, &SilenceInput::default(), &opts);
        assert_eq!(cuts.len(), 2);
        assert_eq!(cuts[0].label, "you know");
        // Phrase spans both words: 0.0-0.04 .. 0.5+0.04.
        assert!((cuts[0].start - 0.0).abs() < 1e-9);
        assert!((cuts[0].end - 0.54).abs() < 1e-9);
        assert_eq!(cuts[1].label, "like");
    }

    #[test]
    fn like_is_kept_outside_aggressive() {
        let words = vec![word("like", 1.0, 1.2, 0.95)];
        let cuts = plan_filler_cuts(
            "en",
            &words,
            &SilenceInput::default(),
            &CutOptions::default(),
        );
        assert!(cuts.is_empty());
    }

    #[test]
    fn non_english_yields_no_cuts() {
        let words = vec![word("um", 1.0, 1.3, 0.95)];
        let cuts = plan_filler_cuts(
            "de",
            &words,
            &SilenceInput::default(),
            &CutOptions::default(),
        );
        assert!(cuts.is_empty());
    }
}
