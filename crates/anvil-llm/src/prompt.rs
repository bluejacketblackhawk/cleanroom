//! The prompts. All of them, in one versioned place.
//!
//! Nothing else in the crate builds prompt text — if a prompt needs to change, it changes
//! here and [`PROMPT_VERSION`] is bumped, so an eval run can be attributed to an exact prompt
//! set (06-QUALITY-EVAL: "shownotes rated usable-without-major-edit ≥80% on 20-episode
//! rubric" is meaningless without knowing which prompts produced it).
//!
//! ## Chat format
//! Qwen2.5-Instruct is a ChatML model. We render the ChatML markup ourselves and run
//! `llama-cli -no-cnv` (raw completion) rather than letting llama.cpp apply its own template:
//! one place owns the exact bytes the model sees, and the same string can be asserted in a
//! unit test.
//!
//! ## The two stages (map-reduce, see [`crate::chunk`])
//! - [`map_prompt`] — one chunk of transcript in, a compact JSON *digest* out
//!   (`summary` / `bullets` / `topics`). Runs once per chunk.
//! - [`reduce_prompt`] — the digests (or, for a short episode, the raw transcript) plus a
//!   list of legal chapter timestamps in, the final [`crate::Shownotes`] JSON out.
//!
//! Both stages demand a bare JSON object. Models still wrap it in prose or a ``` fence
//! sometimes, so [`crate::parse`] extracts the object rather than trusting the model.

use crate::chunk::Chunk;
use crate::TranscriptSegment;

/// Version of this prompt set. Bump on any wording change; eval reports record it.
pub const PROMPT_VERSION: &str = "m4-shownotes-1";

/// ChatML role markers (Qwen2.5).
const IM_START: &str = "<|im_start|>";
const IM_END: &str = "<|im_end|>";

/// Wrap a system+user pair in ChatML and open the assistant turn.
fn chatml(system: &str, user: &str) -> String {
    format!(
        "{IM_START}system\n{system}{IM_END}\n{IM_START}user\n{user}{IM_END}\n{IM_START}assistant\n"
    )
}

const MAP_SYSTEM: &str = "You are a podcast producer's assistant. You read one part of an \
episode transcript and write a compact, factual digest of it. You never invent facts that \
are not in the transcript. You reply with one JSON object and nothing else: no prose before \
it, no markdown fences, no commentary after it.";

const MAP_USER: &str = "Part {INDEX} of {TOTAL} of an episode transcript, covering \
{START} to {END}.

TRANSCRIPT
{BODY}

Write a digest of this part only. Reply with exactly this JSON object:

{
  \"summary\": \"2-4 sentences on what is discussed in this part\",
  \"bullets\": [\"a concrete point actually made in this part\", \"another one\"],
  \"topics\": [\"short topic label\", \"another topic label\"]
}

Rules:
- 2 to 5 bullets, each a full sentence, each about something actually said.
- 2 to 5 topics, each 1-4 words.
- Use the episode's own words for names and jargon; do not translate them.
- No speaker names unless the transcript uses them.
- JSON only.";

const REDUCE_SYSTEM: &str = "You are a podcast producer's assistant. You write the show notes \
a listener reads before pressing play: a short summary, the key points, chapter markers, \
title options and tags. You are accurate before you are clever — you never invent guests, \
sponsors, links, numbers or claims that are not in the material. You reply with one JSON \
object and nothing else: no prose before it, no markdown fences, no commentary after it.";

const REDUCE_USER: &str = "Episode language: {LANGUAGE}. Episode duration: {DURATION}.

MATERIAL
{BODY}

CHAPTER CANDIDATES
Each line is a legal chapter start: a timestamp in seconds and the words spoken there. You \
may only use timestamps from this list.
{CANDIDATES}

Write the show notes. Reply with exactly this JSON object:

{
  \"summary\": \"3-6 sentences a listener reads before pressing play\",
  \"bullets\": [\"key point\", \"key point\", \"key point\"],
  \"chapters\": [{\"title\": \"Chapter title\", \"start\": 0.0}],
  \"titles\": [\"Episode title option\"],
  \"keywords\": [\"tag\", \"tag\", \"tag\"]
}

Rules:
- summary: 3-6 sentences, plain words, no hype, no \"in this episode\" filler.
- bullets: 3 to 7, each a full sentence about something actually discussed.
- chapters: 3 to {MAX_CHAPTERS} of them, in time order, strictly increasing. The first \
chapter starts at {FIRST_START}. `start` is a number of seconds copied verbatim from the \
CHAPTER CANDIDATES list — never a guess, never a clock string, never past {DURATION_SECS}. \
Titles are 2-6 words, descriptive, not \"Part 2\".
- titles: 3 to 5 episode title options, under 70 characters, no clickbait, no emoji.
- keywords: 5 to 10 lowercase tags.
- Write in {LANGUAGE}.
- JSON only.";

/// The map-stage prompt for one transcript chunk.
pub fn map_prompt(chunk: &Chunk, total_chunks: usize) -> String {
    let user = MAP_USER
        .replace("{INDEX}", &(chunk.index + 1).to_string())
        .replace("{TOTAL}", &total_chunks.to_string())
        .replace("{START}", &timecode(chunk.start))
        .replace("{END}", &timecode(chunk.end))
        .replace("{BODY}", chunk.text.trim());
    chatml(MAP_SYSTEM, &user)
}

/// Inputs the reduce stage needs beyond the body text.
#[derive(Debug, Clone)]
pub struct ReduceContext {
    /// Episode language (an ASR-detected code like `en`, or empty).
    pub language: String,
    /// Episode duration in seconds.
    pub duration_secs: f64,
    /// Start time of the first segment — the only legal first-chapter start.
    pub first_start_secs: f64,
    /// Upper bound on chapters we ask for.
    pub max_chapters: usize,
    /// The rendered chapter-candidate lines (see [`chapter_candidates`]).
    pub candidates: String,
}

/// The reduce-stage prompt. `body` is either the joined chunk digests (long episode) or the
/// raw transcript (short episode) — see [`crate::pipeline`].
pub fn reduce_prompt(body: &str, ctx: &ReduceContext) -> String {
    let language = if ctx.language.trim().is_empty() {
        "the language of the transcript"
    } else {
        ctx.language.trim()
    };
    let user = REDUCE_USER
        .replace("{LANGUAGE}", language)
        .replace("{DURATION_SECS}", &format!("{:.1}", ctx.duration_secs))
        .replace("{DURATION}", &timecode(ctx.duration_secs))
        .replace("{FIRST_START}", &format!("{:.1}", ctx.first_start_secs))
        .replace("{MAX_CHAPTERS}", &ctx.max_chapters.to_string())
        .replace("{CANDIDATES}", ctx.candidates.trim())
        .replace("{BODY}", body.trim());
    chatml(REDUCE_SYSTEM, &user)
}

/// Render the chapter-candidate list: `<seconds> | <first words spoken there>`.
///
/// The model may only pick `start` values from these lines, which is what makes the output
/// snappable to real segment boundaries instead of hallucinated clock times. At most
/// `max_lines` candidates are rendered, sampled evenly across the episode (a 2-hour show has
/// far too many segments to list). [`crate::parse::sanitize_chapters`] snaps to the true
/// boundaries afterwards regardless, so this is a quality aid, not a trust assumption.
pub fn chapter_candidates(segments: &[TranscriptSegment], max_lines: usize) -> String {
    if segments.is_empty() || max_lines == 0 {
        return String::new();
    }
    let stride = segments.len().div_ceil(max_lines).max(1);
    let mut out = String::new();
    for seg in segments.iter().step_by(stride) {
        let lead: Vec<&str> = seg.text.split_whitespace().take(12).collect();
        if lead.is_empty() {
            continue;
        }
        out.push_str(&format!("{:.1} | {}\n", seg.start, lead.join(" ")));
    }
    out
}

/// `H:MM:SS` (or `M:SS`) for prompt readability. Never parsed back — the machine-readable
/// times in the prompt are always plain seconds.
pub fn timecode(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs() -> Vec<TranscriptSegment> {
        (0..10)
            .map(|i| TranscriptSegment {
                text: format!("segment number {i} says something about microphones"),
                start: i as f64 * 10.0,
                end: i as f64 * 10.0 + 9.0,
            })
            .collect()
    }

    #[test]
    fn map_prompt_is_chatml_and_carries_the_chunk() {
        let chunk = Chunk {
            index: 1,
            start: 60.0,
            end: 120.0,
            text: "we talked about preamps".into(),
        };
        let p = map_prompt(&chunk, 4);
        assert!(p.starts_with("<|im_start|>system\n"));
        assert!(p.ends_with("<|im_start|>assistant\n"));
        assert_eq!(p.matches(IM_END).count(), 2, "one system + one user turn");
        assert!(p.contains("Part 2 of 4"), "1-based chunk numbering");
        assert!(p.contains("1:00 to 2:00"));
        assert!(p.contains("we talked about preamps"));
        assert!(p.contains("\"topics\""));
    }

    #[test]
    fn reduce_prompt_states_the_chapter_rules_and_lists_candidates() {
        let ctx = ReduceContext {
            language: "en".into(),
            duration_secs: 3_600.0,
            first_start_secs: 0.0,
            max_chapters: 8,
            candidates: chapter_candidates(&segs(), 5),
        };
        let p = reduce_prompt("digest one\ndigest two", &ctx);

        assert!(p.starts_with("<|im_start|>system\n"));
        assert!(p.contains("digest one"));
        assert!(p.contains("Episode duration: 1:00:00"));
        assert!(p.contains("3 to 8 of them"));
        assert!(p.contains("never past 3600.0"));
        assert!(p.contains("The first chapter starts at 0.0"));
        assert!(p.contains("CHAPTER CANDIDATES"));
        // The JSON skeleton survived the placeholder substitution intact.
        assert!(p.contains("\"chapters\": [{\"title\": \"Chapter title\", \"start\": 0.0}]"));
    }

    #[test]
    fn candidates_are_sampled_evenly_and_capped() {
        let rendered = chapter_candidates(&segs(), 4);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() <= 4, "capped, got {}", lines.len());
        // stride = ceil(10/4) = 3 → segments 0, 3, 6, 9 → starts 0, 30, 60, 90.
        assert!(lines[0].starts_with("0.0 | "), "{}", lines[0]);
        assert!(lines[1].starts_with("30.0 | "), "{}", lines[1]);
        assert!(lines[3].starts_with("90.0 | "), "{}", lines[3]);
        // Leads are truncated to a handful of words, not the whole segment.
        assert!(lines[0].split_whitespace().count() <= 15);
    }

    #[test]
    fn candidates_of_an_empty_transcript_are_empty() {
        assert!(chapter_candidates(&[], 10).is_empty());
        assert!(chapter_candidates(&segs(), 0).is_empty());
    }

    #[test]
    fn timecodes_read_like_a_player() {
        assert_eq!(timecode(0.0), "0:00");
        assert_eq!(timecode(9.4), "0:09");
        assert_eq!(timecode(61.0), "1:01");
        assert_eq!(timecode(3_671.0), "1:01:11");
        assert_eq!(timecode(-5.0), "0:00");
    }
}
