//! Chapters, keywords and a summary **without any model** (05-MILESTONES §M4 lane D:
//! "chapter suggestions (topic-drift fallback without LLM)").
//!
//! The LLM pack is a 1–4.7 GB optional download. Most users will never install it, and the
//! Chapters tab must still be useful for them — so this module produces the whole
//! [`Shownotes`] shape from the transcript alone, deterministically, in milliseconds, with
//! zero dependencies beyond the standard library.
//!
//! ## Chapters: lexical cohesion / topic drift (TextTiling)
//! The idea (Hearst, 1997) is that a topic change shows up as a *dip in vocabulary overlap*:
//! while a speaker stays on a subject they keep re-using the same content words; when they
//! move on, the words change with them.
//!
//! 1. **Terms.** Every segment becomes a bag of content terms: lowercased, punctuation
//!    stripped, stopwords and sub-3-character tokens dropped, a crude plural fold applied
//!    (`mics` → `mic`) so the same idea counts as the same term.
//! 2. **Gap scores.** For every possible boundary (the gap before segment *i*) we take a
//!    block of `window` segments on each side and compute the **cosine similarity** of their
//!    term-count vectors. High = the conversation is still about the same thing across the
//!    gap; low = the vocabulary just turned over.
//! 3. **Depth scores.** A raw similarity dip means nothing on its own — some gaps are just
//!    short segments. What matters is how deep the valley is relative to the peaks either
//!    side: `depth = (left_peak − sim) + (right_peak − sim)`, computed only at local minima.
//! 4. **Boundaries.** Keep the minima whose depth clears TextTiling's cutoff (`mean − sd/2`
//!    over the depth scores) *and* an absolute floor ([`FallbackOptions::min_depth`], which
//!    is what stops a single-topic monologue from being chopped into ten chapters). Then take
//!    them strongest-first, skipping any that would sit closer than `min_chapter_secs` to a
//!    boundary already accepted, until `max_chapters` is full.
//! 5. **Titles.** Each chapter's top TF-IDF terms (IDF computed *across chapters*, so a word
//!    the whole episode uses can't title any of them) → `"Microphones, Preamps & Room Noise"`.
//!
//! Chapters land on segment starts by construction, so they are already snapped to the
//! transcript's own boundaries and are strictly increasing and inside the episode.
//!
//! ## Summary, bullets, keywords
//! Extractive, not generative — the fallback never writes a sentence nobody said. Segments
//! are scored by the TF-IDF mass of their terms (length-normalized so a long rambling segment
//! doesn't win by default); the best ones, in time order, become the bullets, and the best
//! few become the summary. Keywords are the episode's top TF-IDF terms. Titles are derived
//! from those keywords and are frankly the weakest part of the fallback — that is the honest
//! trade for not needing 4.7 GB of weights, and the UI labels these as suggestions anyway.

use std::collections::HashMap;

use crate::{Chapter, Shownotes, TranscriptInput, TranscriptSegment};

/// Knobs for the no-LLM path.
#[derive(Debug, Clone)]
pub struct FallbackOptions {
    /// Segments per comparison block either side of a candidate gap. Larger = smoother, less
    /// jumpy boundaries; smaller = more sensitive to short digressions.
    pub window: usize,
    /// Never place two chapters closer than this (seconds). Podcast players are unusable
    /// with 30-second chapters.
    pub min_chapter_secs: f64,
    /// Hard cap on chapters.
    pub max_chapters: usize,
    /// Absolute floor on a boundary's depth score, in cosine-similarity units. Below this the
    /// "topic change" is noise. 0 would make a single-topic episode sprout chapters.
    pub min_depth: f64,
    /// How many bullets to extract.
    pub max_bullets: usize,
    /// How many keywords to extract.
    pub max_keywords: usize,
    /// How many segments the extractive summary is built from.
    pub summary_segments: usize,
}

impl Default for FallbackOptions {
    fn default() -> Self {
        Self {
            window: 3,
            min_chapter_secs: 60.0,
            max_chapters: 10,
            min_depth: 0.18,
            max_bullets: 5,
            max_keywords: 8,
            summary_segments: 3,
        }
    }
}

/// The whole [`Shownotes`] shape, from the transcript alone. Never fails, never allocates a
/// model, never touches the network.
pub fn shownotes(input: &TranscriptInput, opts: &FallbackOptions) -> Shownotes {
    let segments = &input.segments;
    if segments.is_empty() {
        return Shownotes::default();
    }
    let docs: Vec<Terms> = segments.iter().map(|s| terms_of(&s.text)).collect();
    let idf = idf_of(&docs);

    let chapters = chapters_from(segments, &docs, opts);
    let keywords = keywords_from(&docs, &idf, opts.max_keywords);
    let ranked = rank_segments(segments, &docs, &idf);

    let bullets = pick_in_time_order(&ranked, opts.max_bullets)
        .iter()
        .map(|&i| clip(&segments[i].text, 240))
        .collect();
    let summary = pick_in_time_order(&ranked, opts.summary_segments)
        .iter()
        .map(|&i| segments[i].text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    let titles = titles_from(&keywords);

    Shownotes {
        summary: clip(&summary, 700),
        bullets,
        chapters,
        titles,
        keywords,
    }
}

/// Just the chapters (what the Chapters tab's "suggest" button calls when no pack is
/// installed). Deterministic: the same transcript always yields the same chapters.
pub fn chapters(segments: &[TranscriptSegment], opts: &FallbackOptions) -> Vec<Chapter> {
    if segments.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Terms> = segments.iter().map(|s| terms_of(&s.text)).collect();
    chapters_from(segments, &docs, opts)
}

// --- chapters ---------------------------------------------------------------------------

fn chapters_from(
    segments: &[TranscriptSegment],
    docs: &[Terms],
    opts: &FallbackOptions,
) -> Vec<Chapter> {
    let mut starts = vec![0usize];
    starts.extend(topic_boundaries(segments, docs, opts));

    let idf = idf_over_ranges(docs, &starts);
    let mut used: Vec<String> = Vec::new();
    let mut chapters = Vec::with_capacity(starts.len());
    for (n, &from) in starts.iter().enumerate() {
        let to = starts.get(n + 1).copied().unwrap_or(docs.len());
        let title = title_for(&docs[from..to], &idf, n, &mut used);
        chapters.push(Chapter {
            title,
            start: segments[from].start,
        });
    }
    chapters
}

/// Segment indices (never 0) where the vocabulary turns over. See the module docs.
fn topic_boundaries(
    segments: &[TranscriptSegment],
    docs: &[Terms],
    opts: &FallbackOptions,
) -> Vec<usize> {
    let n = docs.len();
    // A window can't exceed a third of the episode, or the blocks either side of a gap
    // overlap the whole show and every gap looks identical.
    let window = opts.window.clamp(1, (n / 3).max(1));
    if n < 2 * window + 1 || opts.max_chapters <= 1 {
        return Vec::new();
    }

    // Candidate gaps: the gap *before* segment i, for i where both blocks are full.
    let gaps: Vec<usize> = (window..=n - window).collect();
    let sims: Vec<f64> = gaps
        .iter()
        .map(|&i| cosine(&block(docs, i - window, i), &block(docs, i, i + window)))
        .collect();

    let depths = depth_scores(&sims);
    let cutoff = depth_cutoff(&depths).max(opts.min_depth);

    // Strongest dips first, each accepted only if it respects min_chapter_secs against the
    // episode start, the episode end, and every boundary already accepted.
    let mut ranked: Vec<usize> = (0..gaps.len()).filter(|&j| depths[j] >= cutoff).collect();
    ranked.sort_by(|&a, &b| depths[b].total_cmp(&depths[a]).then(a.cmp(&b)));

    let episode_start = segments[0].start;
    let episode_end = segments[n - 1].end;
    let mut accepted: Vec<f64> = Vec::new();
    let mut out: Vec<usize> = Vec::new();
    for j in ranked {
        if out.len() + 1 >= opts.max_chapters {
            break;
        }
        let seg = gaps[j];
        let t = segments[seg].start;
        let far_enough = t - episode_start >= opts.min_chapter_secs
            && episode_end - t >= opts.min_chapter_secs
            && accepted
                .iter()
                .all(|&a| (t - a).abs() >= opts.min_chapter_secs);
        if far_enough {
            accepted.push(t);
            out.push(seg);
        }
    }
    out.sort_unstable();
    out
}

/// TextTiling depth: at each local minimum, how far the similarity fell from the peaks on
/// either side. Non-minima score 0.
fn depth_scores(sims: &[f64]) -> Vec<f64> {
    let mut depths = vec![0.0; sims.len()];
    for i in 0..sims.len() {
        let left_lower = i == 0 || sims[i - 1] >= sims[i];
        let right_lower = i + 1 == sims.len() || sims[i + 1] >= sims[i];
        if !(left_lower && right_lower) {
            continue;
        }
        // Climb left while the curve keeps rising; the highest point reached is the peak.
        let mut peak_l = sims[i];
        for j in (0..i).rev() {
            if sims[j] < peak_l {
                break;
            }
            peak_l = sims[j];
        }
        let mut peak_r = sims[i];
        for &s in sims.iter().skip(i + 1) {
            if s < peak_r {
                break;
            }
            peak_r = s;
        }
        depths[i] = (peak_l - sims[i]) + (peak_r - sims[i]);
    }
    depths
}

/// TextTiling's cutoff: `mean − sd/2` over the non-zero depth scores.
fn depth_cutoff(depths: &[f64]) -> f64 {
    let scored: Vec<f64> = depths.iter().copied().filter(|d| *d > 0.0).collect();
    if scored.is_empty() {
        return f64::INFINITY;
    }
    let mean = scored.iter().sum::<f64>() / scored.len() as f64;
    let var = scored.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / scored.len() as f64;
    mean - var.sqrt() / 2.0
}

// --- terms, tf-idf ----------------------------------------------------------------------

type Terms = HashMap<String, f64>;

/// Merge the term counts of `docs[from..to]` into one block vector.
fn block(docs: &[Terms], from: usize, to: usize) -> Terms {
    let mut out: Terms = HashMap::new();
    for doc in &docs[from..to] {
        for (term, count) in doc {
            *out.entry(term.clone()).or_insert(0.0) += count;
        }
    }
    out
}

fn cosine(a: &Terms, b: &Terms) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (term, x) in a {
        na += x * x;
        if let Some(y) = b.get(term) {
            dot += x * y;
        }
    }
    for y in b.values() {
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Smoothed inverse document frequency, `ln(1 + N/df)`, over the segments as documents.
///
/// Smoothed, not the textbook `ln(N/df)`, because that collapses to **zero** for any term
/// that appears in every document — and a single-topic episode is a transcript where *every*
/// term does. Textbook IDF would score every segment 0, and the extractive summary would come
/// back empty on exactly the episodes it is most needed for. The `1 +` floors IDF at `ln 2`
/// while preserving the ranking that matters: rarer term, higher weight.
fn idf_of(docs: &[Terms]) -> HashMap<String, f64> {
    let n = docs.len().max(1) as f64;
    let mut df: HashMap<String, f64> = HashMap::new();
    for doc in docs {
        for term in doc.keys() {
            *df.entry(term.clone()).or_insert(0.0) += 1.0;
        }
    }
    df.into_iter()
        .map(|(term, d)| (term, (1.0 + n / d).ln()))
        .collect()
}

/// Same, but with each *chapter* as a document — so a term the whole episode says (the show's
/// own subject) can't title every chapter.
fn idf_over_ranges(docs: &[Terms], starts: &[usize]) -> HashMap<String, f64> {
    let chapters: Vec<Terms> = starts
        .iter()
        .enumerate()
        .map(|(n, &from)| {
            let to = starts.get(n + 1).copied().unwrap_or(docs.len());
            block(docs, from, to)
        })
        .collect();
    idf_of(&chapters)
}

/// Top TF-IDF terms of a whole episode.
fn keywords_from(docs: &[Terms], idf: &HashMap<String, f64>, max: usize) -> Vec<String> {
    let mut totals: HashMap<&str, f64> = HashMap::new();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for doc in docs {
        for (term, count) in doc {
            *totals.entry(term).or_insert(0.0) += count * idf.get(term).copied().unwrap_or(0.0);
            *df.entry(term).or_insert(0) += 1;
        }
    }
    // A term said exactly once in a long transcript is usually an ASR misfire, not a tag.
    let min_df = if docs.len() >= 10 { 2 } else { 1 };
    let mut ranked: Vec<(&str, f64)> = totals
        .into_iter()
        .filter(|(t, _)| df.get(t).copied().unwrap_or(0) >= min_df)
        .collect();
    // Score descending, then alphabetically — ties must not depend on HashMap iteration
    // order, or the "suggest" button would give a different answer every click.
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    ranked
        .into_iter()
        .take(max)
        .map(|(t, _)| t.to_string())
        .collect()
}

/// Segment indices, best TF-IDF mass first (length-normalized).
fn rank_segments(
    segments: &[TranscriptSegment],
    docs: &[Terms],
    idf: &HashMap<String, f64>,
) -> Vec<usize> {
    let mut scored: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            let total: f64 = doc.len() as f64;
            let mass: f64 = doc
                .iter()
                .map(|(t, c)| c * idf.get(t).copied().unwrap_or(0.0))
                .sum();
            // Too-short segments ("Right." "Exactly.") make terrible bullets whatever their
            // vocabulary, so they are scored out rather than special-cased later.
            let words = segments[i].text.split_whitespace().count();
            let score = if words < 6 {
                0.0
            } else {
                mass / total.max(1.0).sqrt()
            };
            (i, score)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .filter(|(_, s)| *s > 0.0)
        .map(|(i, _)| i)
        .collect()
}

/// The best `n` of `ranked`, put back in time order.
fn pick_in_time_order(ranked: &[usize], n: usize) -> Vec<usize> {
    let mut picked: Vec<usize> = ranked.iter().copied().take(n).collect();
    picked.sort_unstable();
    picked
}

/// A chapter title from its top TF-IDF terms, deduped against titles already used.
fn title_for(
    docs: &[Terms],
    idf: &HashMap<String, f64>,
    index: usize,
    used: &mut Vec<String>,
) -> String {
    let merged = block(docs, 0, docs.len());
    let mut ranked: Vec<(&String, f64)> = merged
        .iter()
        .map(|(t, c)| (t, c * idf.get(t).copied().unwrap_or(0.0)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    // Belt and braces: if nothing scored (a chapter of pure stopwords), rank by raw frequency
    // rather than giving up and calling it "Chapter 1".
    if ranked.is_empty() {
        ranked = merged.iter().map(|(t, c)| (t, *c)).collect();
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));

    let terms: Vec<String> = ranked
        .iter()
        .take(3)
        .map(|(t, _)| title_case(t))
        .collect::<Vec<_>>();

    let mut title = join_terms(&terms);
    if title.is_empty() {
        title = format!("Chapter {}", index + 1);
    }
    if used.contains(&title) {
        title = format!("{title} (cont.)");
    }
    used.push(title.clone());
    title
}

/// `["Mics", "Preamps", "Room"]` → `"Mics, Preamps & Room"`.
fn join_terms(terms: &[String]) -> String {
    match terms {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} & {last}", rest.join(", ")),
    }
}

/// Episode-title candidates from the top keywords. Keyword-shaped, not editorial — see the
/// module docs.
fn titles_from(keywords: &[String]) -> Vec<String> {
    let tc: Vec<String> = keywords.iter().take(4).map(|k| title_case(k)).collect();
    let mut out = Vec::new();
    match tc.as_slice() {
        [] => {}
        [a] => out.push(a.clone()),
        [a, b, rest @ ..] => {
            out.push(join_terms(&tc[..tc.len().min(3)]));
            out.push(format!("{a} and {b}"));
            if let Some(c) = rest.first() {
                out.push(format!("{a}, {b}, {c}"));
            }
        }
    }
    out.dedup();
    out
}

fn title_case(term: &str) -> String {
    let mut chars = term.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Trim `text` to at most `max` characters on a word boundary.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    match head.rsplit_once(' ') {
        Some((cut, _)) => format!("{}…", cut.trim_end_matches([',', ';', ':'])),
        None => format!("{head}…"),
    }
}

/// Content terms of a segment: lowercase, de-punctuated, stopworded, plural-folded.
fn terms_of(text: &str) -> Terms {
    let mut out: Terms = HashMap::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '\'') {
        let token: String = raw
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        if token.len() < 3 || token.chars().all(|c| c.is_numeric()) || is_stopword(&token) {
            continue;
        }
        *out.entry(fold_plural(&token)).or_insert(0.0) += 1.0;
    }
    out
}

/// `mics` → `mic`, `buses` → `buse`… crude, but consistent, which is all cosine similarity
/// needs. Guards the obvious traps (`bass`, `focus`, `analysis`).
fn fold_plural(token: &str) -> String {
    if token.len() > 3
        && token.ends_with('s')
        && !token.ends_with("ss")
        && !token.ends_with("us")
        && !token.ends_with("is")
    {
        return token[..token.len() - 1].to_string();
    }
    token.to_string()
}

/// English function words + the verbal tics a transcript is full of ("yeah", "kind of").
/// Terms shorter than 3 characters are already dropped, so this list only needs the longer
/// ones.
const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "all",
    "any",
    "can",
    "had",
    "her",
    "was",
    "one",
    "our",
    "out",
    "day",
    "get",
    "has",
    "him",
    "his",
    "how",
    "man",
    "new",
    "now",
    "old",
    "see",
    "two",
    "way",
    "who",
    "boy",
    "did",
    "its",
    "let",
    "put",
    "say",
    "she",
    "too",
    "use",
    "that",
    "with",
    "have",
    "this",
    "will",
    "your",
    "from",
    "they",
    "know",
    "want",
    "been",
    "good",
    "much",
    "some",
    "time",
    "very",
    "when",
    "come",
    "here",
    "just",
    "like",
    "long",
    "make",
    "many",
    "more",
    "only",
    "over",
    "such",
    "take",
    "than",
    "them",
    "well",
    "were",
    "what",
    "about",
    "there",
    "think",
    "would",
    "these",
    "thing",
    "things",
    "really",
    "going",
    "yeah",
    "okay",
    "right",
    "actually",
    "basically",
    "kind",
    "sort",
    "mean",
    "maybe",
    "gonna",
    "something",
    "anything",
    "everything",
    "because",
    "could",
    "should",
    "still",
    "guess",
    "little",
    "lot",
    "bit",
    "way",
    "said",
    "says",
    "one",
    "also",
    "into",
    "even",
    "back",
    "look",
    "looking",
    "talk",
    "talking",
    "talked",
    "let's",
    "don't",
    "didn't",
    "doesn't",
    "it's",
    "that's",
    "we're",
    "you're",
    "i'm",
    "isn't",
    "can't",
    "won't",
    "they're",
    "i've",
    "we've",
    "you've",
    "there's",
    "got",
    "get",
    "getting",
    "goes",
    "went",
    "does",
    "doing",
    "done",
    "made",
    "makes",
    "making",
    "give",
    "gives",
    "given",
    "need",
    "needs",
    "want",
    "wants",
    "wanted",
];

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic 30-minute, three-topic episode: microphones → room acoustics → editing
    /// software. Each topic keeps a stable vocabulary and shares almost none with the others,
    /// which is exactly the signal topic-drift detection exists to find.
    fn three_topic_episode() -> TranscriptInput {
        let mic = [
            "the dynamic microphone rejects the room and the cardioid capsule helps as well",
            "a condenser microphone hears every reflection in the room so the capsule matters",
            "we prefer a dynamic microphone with a tight cardioid pattern for spoken voice",
            "microphone technique matters more than the microphone price for a clean voice",
            "keep the microphone close to the mouth and the capsule slightly off axis",
            "a good dynamic microphone with a cardioid capsule forgives a noisy voice",
            "the microphone preamp adds clean gain before the capsule signal reaches the box",
            "cheap microphone preamp hiss is audible when the microphone gain is high",
        ];
        let room = [
            "the room reflections smear the voice and the reverb tail sounds boxy",
            "acoustic panels on the wall absorb reflections and shorten the reverb tail",
            "a carpet plus curtains kill early reflections in an untreated square room",
            "the reverb tail in an empty room makes speech sound distant and boxy",
            "treat the wall behind the speaker first because those reflections arrive first",
            "acoustic foam is thin and only absorbs the top end of the reverb tail",
            "a bookshelf scatters reflections and tames the boxy sound of a small room",
            "measuring the reverb tail of the room shows how much absorption the wall needs",
        ];
        let edit = [
            "the editing software timeline lets you cut filler words from the track",
            "an editing session is faster when the software shows the transcript as text",
            "cut the silence in the editing timeline and the episode tightens up",
            "some editing software renders the export in the background while you cut",
            "the transcript view in modern editing software makes cutting filler quick",
            "render the export from the editing timeline once the cuts are approved",
            "keyboard shortcuts in the editing software beat clicking through the timeline",
            "back up the editing project before the final export render finishes",
        ];
        let mut segments = Vec::new();
        let mut t = 0.0;
        for text in mic.iter().chain(room.iter()).chain(edit.iter()) {
            segments.push(TranscriptSegment {
                text: (*text).to_string(),
                start: t,
                end: t + 70.0,
            });
            t += 75.0;
        }
        TranscriptInput {
            language: "en".into(),
            segments,
        }
    }

    #[test]
    fn topic_drift_finds_the_two_real_boundaries() {
        let episode = three_topic_episode();
        let chapters = chapters(&episode.segments, &FallbackOptions::default());

        assert_eq!(
            chapters.len(),
            3,
            "three topics → three chapters, got {chapters:#?}"
        );
        // Topic 2 starts at segment 8 (600 s), topic 3 at segment 16 (1200 s). The detector
        // is allowed to be a segment early or late; it is not allowed to be lost.
        assert_eq!(chapters[0].start, 0.0);
        assert!(
            (525.0..=675.0).contains(&chapters[1].start),
            "boundary 1 at {}, expected ≈600",
            chapters[1].start
        );
        assert!(
            (1125.0..=1275.0).contains(&chapters[2].start),
            "boundary 2 at {}, expected ≈1200",
            chapters[2].start
        );
    }

    #[test]
    fn chapter_titles_come_from_the_vocabulary_of_each_chapter() {
        let episode = three_topic_episode();
        let chapters = chapters(&episode.segments, &FallbackOptions::default());
        let titles: Vec<String> = chapters.iter().map(|c| c.title.to_lowercase()).collect();

        assert!(titles[0].contains("microphone"), "{:?}", titles[0]);
        assert!(
            titles[1].contains("reflection") || titles[1].contains("reverb"),
            "{:?}",
            titles[1]
        );
        assert!(
            titles[2].contains("editing") || titles[2].contains("software"),
            "{:?}",
            titles[2]
        );
        // Titles are distinct and human-shaped, not "Chapter 2".
        assert!(titles.iter().all(|t| !t.starts_with("chapter ")));
    }

    #[test]
    fn chapters_obey_the_contract_the_chapter_writer_depends_on() {
        let episode = three_topic_episode();
        let duration = episode.duration_secs();
        let chapters = chapters(&episode.segments, &FallbackOptions::default());
        let starts: Vec<f64> = episode.segments.iter().map(|s| s.start).collect();

        for pair in chapters.windows(2) {
            assert!(pair[1].start > pair[0].start, "strictly increasing");
            assert!(pair[1].start - pair[0].start >= 60.0, "min chapter length");
        }
        for c in &chapters {
            assert!((0.0..=duration).contains(&c.start), "inside the episode");
            assert!(starts.contains(&c.start), "snapped to a segment boundary");
            assert!(!c.title.trim().is_empty());
        }
    }

    #[test]
    fn a_single_topic_monologue_is_not_chopped_up() {
        // Same vocabulary throughout: there is no topic drift to find, and inventing
        // boundaries anyway would be worse than useless.
        let segments: Vec<TranscriptSegment> = (0..24)
            .map(|i| TranscriptSegment {
                text: "the compressor threshold and the ratio control the compressor gain \
                       reduction on the voice"
                    .into(),
                start: i as f64 * 75.0,
                end: i as f64 * 75.0 + 70.0,
            })
            .collect();
        let chapters = chapters(&segments, &FallbackOptions::default());
        assert_eq!(chapters.len(), 1, "got {chapters:#?}");
        assert_eq!(chapters[0].start, 0.0);
    }

    #[test]
    fn options_bound_the_result() {
        let episode = three_topic_episode();
        let opts = FallbackOptions {
            max_chapters: 2,
            ..Default::default()
        };
        assert_eq!(chapters(&episode.segments, &opts).len(), 2);

        // A min length longer than the episode leaves exactly the opening chapter.
        let opts = FallbackOptions {
            min_chapter_secs: 10_000.0,
            ..Default::default()
        };
        assert_eq!(chapters(&episode.segments, &opts).len(), 1);
    }

    #[test]
    fn short_and_empty_transcripts_do_not_panic() {
        assert!(chapters(&[], &FallbackOptions::default()).is_empty());
        let one = [TranscriptSegment {
            text: "hello and welcome to the show".into(),
            start: 0.0,
            end: 4.0,
        }];
        let chapters = chapters(&one, &FallbackOptions::default());
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].start, 0.0);

        let empty = shownotes(&TranscriptInput::default(), &FallbackOptions::default());
        assert_eq!(empty, Shownotes::default());
    }

    #[test]
    fn the_whole_shownotes_shape_is_filled_without_a_model() {
        let episode = three_topic_episode();
        let notes = shownotes(&episode, &FallbackOptions::default());

        assert!(!notes.summary.is_empty());
        assert!(notes.summary.len() <= 700);
        assert_eq!(notes.bullets.len(), 5);
        assert!(notes.bullets.iter().all(|b| !b.trim().is_empty()));
        assert_eq!(notes.chapters.len(), 3);
        assert!(!notes.titles.is_empty());
        assert!(notes.keywords.len() >= 5);
        // Keywords are the episode's real vocabulary, not stopwords.
        assert!(
            notes
                .keywords
                .iter()
                .any(|k| k.contains("microphone") || k.contains("editing") || k.contains("room")),
            "{:?}",
            notes.keywords
        );
        assert!(notes
            .keywords
            .iter()
            .all(|k| !STOPWORDS.contains(&k.as_str())));
    }

    #[test]
    fn it_is_deterministic() {
        let episode = three_topic_episode();
        let a = shownotes(&episode, &FallbackOptions::default());
        let b = shownotes(&episode, &FallbackOptions::default());
        assert_eq!(a, b);
    }

    #[test]
    fn terms_drop_stopwords_and_fold_plurals() {
        let t = terms_of("The microphones and the MICROPHONE, really!");
        assert_eq!(t.get("microphone"), Some(&2.0));
        assert!(!t.contains_key("the"));
        assert!(!t.contains_key("really"));
        // Traps the plural fold must not fall into.
        assert_eq!(fold_plural("bass"), "bass");
        assert_eq!(fold_plural("focus"), "focus");
        assert_eq!(fold_plural("analysis"), "analysis");
        assert_eq!(fold_plural("mics"), "mic");
    }

    #[test]
    fn cosine_is_one_for_identical_and_zero_for_disjoint() {
        let a = terms_of("microphone preamp capsule");
        let b = terms_of("microphone preamp capsule");
        let c = terms_of("reverb curtains carpet");
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-9);
        assert_eq!(cosine(&a, &c), 0.0);
        assert_eq!(cosine(&a, &Terms::new()), 0.0);
    }
}
