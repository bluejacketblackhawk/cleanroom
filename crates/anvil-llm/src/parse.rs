//! Turning what the model *actually* emits into the [`Shownotes`] contract.
//!
//! A 4-bit 7B model asked for "JSON only" complies most of the time. The rest of the time it
//! wraps the object in a ```json fence, prefaces it with "Sure, here are the show notes:",
//! writes `"start": "12:34"` instead of a number, invents a chapter at 99:00 of a 40-minute
//! episode, or repeats a timestamp twice. None of that is allowed to reach the UI, and none
//! of it is worth a failed job either.
//!
//! So this module is deliberately forgiving on the way in and strict on the way out:
//! - [`extract_json`] finds the first balanced `{…}` object in a pile of text.
//! - [`ChunkDigest`] / [`RawShownotes`] deserialize leniently (missing fields default,
//!   `start` accepts a number *or* a `"1:02:03"` clock string).
//! - [`sanitize_chapters`] then enforces the contract the rest of Cleanroom relies on: snapped to
//!   real segment boundaries, strictly increasing, inside `[first_start, duration]`, titled,
//!   capped.
//!
//! Everything here is pure string → struct. No model, no process: fully unit-testable, and
//! tested against exactly the mangled outputs above.

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::{Chapter, Shownotes, TranscriptSegment};

/// The map stage's per-chunk digest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkDigest {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
}

/// The reduce stage's raw output, before sanitizing. Every field defaults, so a model that
/// omits `keywords` yields empty keywords rather than a hard parse error.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawShownotes {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub chapters: Vec<RawChapter>,
    #[serde(default)]
    pub titles: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// A chapter as the model wrote it: `start` may be a number of seconds or a clock string.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawChapter {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub start: RawTime,
}

/// `start` as it arrives: `12.5`, `"12.5"`, `"1:02:03"`, or missing.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawTime {
    Secs(f64),
    Text(String),
}

impl Default for RawTime {
    fn default() -> Self {
        RawTime::Secs(f64::NAN)
    }
}

impl RawTime {
    /// Seconds, or `None` if it is unparseable (in which case the chapter is dropped — a
    /// chapter marker at an unknown time is worse than no chapter marker).
    pub fn secs(&self) -> Option<f64> {
        match self {
            RawTime::Secs(s) if s.is_finite() => Some(*s),
            RawTime::Secs(_) => None,
            RawTime::Text(t) => parse_clock(t),
        }
    }
}

/// Parse `"93.5"`, `"1:33"`, `"1:02:03"` (and `"00:01:33,500"`) into seconds.
pub fn parse_clock(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches('s').trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(secs) = text.parse::<f64>() {
        return secs.is_finite().then_some(secs);
    }
    let mut total = 0.0f64;
    let mut parts = 0usize;
    for part in text.split(':') {
        let part = part.trim().replace(',', ".");
        let value: f64 = part.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total = total * 60.0 + value;
        parts += 1;
        if parts > 3 {
            return None;
        }
    }
    (parts >= 2).then_some(total)
}

/// The first balanced JSON object in `raw`, ignoring braces inside strings.
///
/// Tolerates a ```json fence, a prose preamble, a trailing `[end of text]`, and any
/// commentary after the object.
pub fn extract_json(raw: &str) -> Result<&str, LlmError> {
    let bytes = raw.as_bytes();
    let start = raw
        .find('{')
        .ok_or_else(|| LlmError::NoJson(tail(raw, 200)))?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    // Unbalanced: the generation was cut off by the token budget.
    Err(LlmError::NoJson(format!(
        "unterminated JSON object (generation truncated?): {}",
        tail(&raw[start..], 200)
    )))
}

/// Parse a map-stage digest out of raw model output.
pub fn parse_digest(raw: &str) -> Result<ChunkDigest, LlmError> {
    let json = extract_json(raw)?;
    let mut digest: ChunkDigest = serde_json::from_str(json)?;
    digest.bullets.retain(|b| !b.trim().is_empty());
    digest.topics.retain(|t| !t.trim().is_empty());
    Ok(digest)
}

/// Rules the sanitizer enforces on the model's chapters.
#[derive(Debug, Clone)]
pub struct ChapterRules {
    /// Legal chapter starts: every segment boundary (a chapter that lands mid-sentence is
    /// snapped to the nearest one).
    pub boundaries: Vec<f64>,
    /// Episode duration in seconds; chapters at or past it are dropped.
    pub duration_secs: f64,
    /// Chapters closer together than this are collapsed (keeping the earlier one).
    pub min_gap_secs: f64,
    /// Hard cap on the number of chapters kept.
    pub max_chapters: usize,
}

impl ChapterRules {
    /// Rules derived from the transcript itself.
    pub fn from_segments(
        segments: &[TranscriptSegment],
        min_gap_secs: f64,
        max_chapters: usize,
    ) -> Self {
        Self {
            boundaries: segments.iter().map(|s| s.start).collect(),
            duration_secs: segments.last().map(|s| s.end).unwrap_or(0.0),
            min_gap_secs,
            max_chapters,
        }
    }
}

/// Enforce the chapter contract on whatever the model produced.
///
/// In order: drop untimed/untitled chapters → snap each `start` to the nearest real segment
/// boundary → drop anything past the episode duration → sort → collapse duplicates and
/// too-close neighbours → force the first chapter to the transcript's first boundary → cap.
/// The result is always strictly increasing and inside the episode; that is a contract the
/// chapter writer (`anvil-media` ID3 CHAP / MP4 atoms) depends on.
pub fn sanitize_chapters(raw: &[RawChapter], rules: &ChapterRules) -> Vec<Chapter> {
    let mut chapters: Vec<Chapter> = raw
        .iter()
        .filter_map(|c| {
            let title = clean_title(&c.title);
            let start = c.start.secs()?;
            // Range-check *before* snapping: a hallucinated 99:99:99 must be dropped, not
            // snapped back onto the last real boundary.
            if title.is_empty() || !start.is_finite() || start < 0.0 || start > rules.duration_secs
            {
                return None;
            }
            Some(Chapter {
                title,
                start: snap(start, &rules.boundaries),
            })
        })
        .collect();

    chapters.sort_by(|a, b| a.start.total_cmp(&b.start));

    let mut kept: Vec<Chapter> = Vec::new();
    for chapter in chapters {
        match kept.last() {
            Some(prev) if chapter.start - prev.start < rules.min_gap_secs => continue,
            _ => kept.push(chapter),
        }
    }

    // The first chapter must open the episode — a show whose first marker is at 4:12 leaves
    // the first four minutes unchaptered in every player.
    if let (Some(first_boundary), Some(first)) = (rules.boundaries.first(), kept.first_mut()) {
        first.start = *first_boundary;
    }
    kept.truncate(rules.max_chapters.max(1));
    kept
}

/// Parse the reduce stage's output into a finished [`Shownotes`].
pub fn parse_shownotes(raw: &str, rules: &ChapterRules) -> Result<Shownotes, LlmError> {
    let json = extract_json(raw)?;
    let parsed: RawShownotes = serde_json::from_str(json)?;
    Ok(Shownotes {
        summary: parsed.summary.trim().to_string(),
        bullets: clean_list(parsed.bullets),
        chapters: sanitize_chapters(&parsed.chapters, rules),
        titles: clean_list(parsed.titles),
        keywords: clean_keywords(parsed.keywords),
    })
}

/// Nearest value in `boundaries` (assumed ascending). `start` unchanged if there are none.
fn snap(start: f64, boundaries: &[f64]) -> f64 {
    boundaries
        .iter()
        .copied()
        .min_by(|a, b| (a - start).abs().total_cmp(&(b - start).abs()))
        .unwrap_or(start)
}

/// Trim a title and strip the decorations models add ("Chapter 3: ", markdown, quotes).
fn clean_title(title: &str) -> String {
    let mut t = title.trim().trim_matches(['"', '*', '#', '-']).trim();
    for prefix in ["Chapter ", "Section ", "Part "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            // "Chapter 3: Mic technique" → "Mic technique"; "Chapter of Errors" is left be.
            if let Some((num, after)) = rest.split_once(':') {
                if num.trim().chars().all(|c| c.is_ascii_digit()) && !after.trim().is_empty() {
                    t = after.trim();
                }
            }
        }
    }
    t.to_string()
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = items
        .into_iter()
        .map(|s| {
            s.trim()
                .trim_start_matches(['-', '*', '•'])
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    out.dedup();
    out
}

fn clean_keywords(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = clean_list(items)
        .into_iter()
        .map(|k| k.trim_start_matches('#').trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    out.dedup();
    out
}

fn tail(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    t.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments() -> Vec<TranscriptSegment> {
        (0..40)
            .map(|i| TranscriptSegment {
                text: format!("segment {i}"),
                start: i as f64 * 30.0,
                end: i as f64 * 30.0 + 29.0,
            })
            .collect()
    }

    fn rules() -> ChapterRules {
        ChapterRules::from_segments(&segments(), 45.0, 8)
    }

    #[test]
    fn extracts_json_from_a_fenced_chatty_answer() {
        let raw = "Sure! Here are the show notes:\n\n```json\n{\n  \"summary\": \"a { brace } \
                   in a string\",\n  \"bullets\": [\"one\"]\n}\n```\nHope that helps!\n\
                   [end of text]";
        let json = extract_json(raw).expect("extract");
        assert!(json.starts_with('{') && json.ends_with('}'));
        let parsed: RawShownotes = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.summary, "a { brace } in a string");
        assert_eq!(parsed.bullets, ["one"]);
    }

    #[test]
    fn escaped_quotes_do_not_confuse_the_extractor() {
        let raw = r#"{"summary": "he said \"hi\" and left {"}"#;
        let json = extract_json(raw).expect("extract");
        let parsed: RawShownotes = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.summary, r#"he said "hi" and left {"#);
    }

    #[test]
    fn truncated_and_missing_json_are_errors_not_panics() {
        assert!(matches!(
            extract_json("I'd rather not."),
            Err(LlmError::NoJson(_))
        ));
        assert!(matches!(
            extract_json("{\"summary\": \"cut off mid-gene"),
            Err(LlmError::NoJson(_))
        ));
    }

    #[test]
    fn clock_strings_parse_like_a_player_shows_them() {
        assert_eq!(parse_clock("93.5"), Some(93.5));
        assert_eq!(parse_clock("1:33"), Some(93.0));
        assert_eq!(parse_clock("01:02:03"), Some(3_723.0));
        assert_eq!(parse_clock("00:01:33,500"), Some(93.5));
        assert_eq!(parse_clock("12s"), Some(12.0));
        assert_eq!(parse_clock("later"), None);
        assert_eq!(parse_clock(""), None);
        assert_eq!(parse_clock("1:2:3:4"), None);
    }

    #[test]
    fn chapters_are_snapped_sorted_deduped_and_clamped() {
        // Deliberately awful: out of order, mid-segment, past the end, duplicated, untitled,
        // clock-string, negative, and one titled "Chapter 2: …".
        let raw: Vec<RawChapter> = serde_json::from_str(
            r#"[
              {"title": "Room noise",       "start": 612.7},
              {"title": "Cold open",        "start": 3.0},
              {"title": "",                 "start": 700.0},
              {"title": "Beyond the end",   "start": 99999.0},
              {"title": "Chapter 2: Mics",  "start": "5:02"},
              {"title": "Dup of room noise","start": 620.0},
              {"title": "Negative",         "start": -5.0},
              {"title": "Wrap up",          "start": 1080.0}
            ]"#,
        )
        .expect("fixture parses");

        let chapters = sanitize_chapters(&raw, &rules());
        let starts: Vec<f64> = chapters.iter().map(|c| c.start).collect();
        let titles: Vec<&str> = chapters.iter().map(|c| c.title.as_str()).collect();

        // Snapped to 30 s segment boundaries; the 620 s duplicate collapsed into 610 s
        // (min gap 45 s); the untitled, negative and out-of-range ones are gone.
        assert_eq!(starts, [0.0, 300.0, 600.0, 1080.0]);
        assert_eq!(titles, ["Cold open", "Mics", "Room noise", "Wrap up"]);

        // The contract downstream depends on.
        for pair in chapters.windows(2) {
            assert!(pair[1].start > pair[0].start, "strictly increasing");
        }
        assert_eq!(chapters[0].start, 0.0, "first chapter opens the episode");
        assert!(chapters.iter().all(|c| c.start <= 1_199.0));
    }

    #[test]
    fn chapters_are_capped() {
        let raw: Vec<RawChapter> = (0..30)
            .map(|i| RawChapter {
                title: format!("t{i}"),
                start: RawTime::Secs(i as f64 * 60.0),
            })
            .collect();
        let mut rules = rules();
        rules.max_chapters = 5;
        assert_eq!(sanitize_chapters(&raw, &rules).len(), 5);
    }

    #[test]
    fn a_realistic_model_answer_becomes_shownotes() {
        let raw = "```json\n{\n\
          \"summary\": \"Two engineers talk about recording a podcast in a bad room.\",\n\
          \"bullets\": [\"- Dynamic mics forgive a live room.\", \"\", \"Treat the wall first.\"],\n\
          \"chapters\": [\n\
            {\"title\": \"Intro\", \"start\": 12.0},\n\
            {\"title\": \"Microphones\", \"start\": \"5:00\"},\n\
            {\"title\": \"Room treatment\", \"start\": 900}\n\
          ],\n\
          \"titles\": [\"Recording in a bad room\", \"\"],\n\
          \"keywords\": [\"#Microphones\", \"acoustics\", \"acoustics\"]\n\
        }\n```";

        let notes = parse_shownotes(raw, &rules()).expect("parse");
        assert!(notes.summary.starts_with("Two engineers"));
        // Empty bullets dropped, list markers stripped.
        assert_eq!(
            notes.bullets,
            ["Dynamic mics forgive a live room.", "Treat the wall first."]
        );
        assert_eq!(notes.titles, ["Recording in a bad room"]);
        // Keywords: lowercased, de-hashed, deduped.
        assert_eq!(notes.keywords, ["microphones", "acoustics"]);
        assert_eq!(
            notes.chapters,
            vec![
                Chapter {
                    title: "Intro".into(),
                    start: 0.0
                },
                Chapter {
                    title: "Microphones".into(),
                    start: 300.0
                },
                Chapter {
                    title: "Room treatment".into(),
                    start: 900.0
                },
            ]
        );
    }

    #[test]
    fn a_digest_survives_missing_fields() {
        let d = parse_digest("{\"summary\": \"they talked\"}").expect("parse");
        assert_eq!(d.summary, "they talked");
        assert!(d.bullets.is_empty());
        assert!(d.topics.is_empty());
    }
}
