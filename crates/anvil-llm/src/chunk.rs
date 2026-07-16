//! Chunking a transcript so a 2-hour episode fits an 8k context window.
//!
//! ## The budget
//! A 2-hour podcast is roughly 20,000 words ≈ 26k tokens — several times the context we run
//! Qwen2.5 with by default (8k, chosen to bound KV-cache RAM on an 8 GB machine, not because
//! the model can't do more: it does 32k). So the transcript is split into chunks that each
//! leave room for the prompt scaffolding *and* the model's answer:
//!
//! ```text
//! chunk_budget_tokens = ctx_tokens - RESERVE_PROMPT_TOKENS - max_output_tokens
//! chunk_budget_chars  = chunk_budget_tokens * CHARS_PER_TOKEN
//! ```
//!
//! Token counts are **estimated from characters** ([`estimate_tokens`]), not tokenized — the
//! real tokenizer lives inside llama.cpp behind the sidecar boundary. The estimate is
//! deliberately pessimistic (3.5 chars/token vs ~4 for English prose) so the estimate errs
//! toward smaller chunks; llama.cpp would otherwise silently truncate the *front* of an
//! overlong prompt, which is exactly where our instructions live.
//!
//! ## Chunk boundaries
//! Chunks are packed greedily out of whole [`TranscriptSegment`]s — never split mid-segment,
//! so every chunk boundary is also a legal chapter boundary and every chunk has honest
//! start/end times. Consecutive chunks **overlap** by [`OVERLAP_FRACTION`] of the budget
//! (whole segments again), so a topic that straddles a boundary is visible in both chunks
//! and the map stage doesn't cut a thought in half. A single segment longer than the whole
//! budget (an ASR pathology — no punctuation for ten minutes) becomes its own oversized
//! chunk rather than being cut mid-word; llama.cpp will truncate it, and that is the least
//! bad outcome.
//!
//! ## Map-reduce
//! [`crate::pipeline`] summarizes each chunk (map), then summarizes the summaries (reduce).
//! One chunk → the reduce stage reads the transcript directly and the map stage is skipped.

use crate::TranscriptSegment;

/// Pessimistic characters-per-token for English-ish transcript text.
pub const CHARS_PER_TOKEN: f64 = 3.5;

/// Tokens held back for the prompt scaffolding: system + instructions + JSON skeleton
/// (~450 tokens) and, in the reduce prompt, up to 80 chapter-candidate lines (~1,600). The
/// worst case is a single-chunk episode, where the reduce prompt carries the whole transcript
/// *and* the candidate list; this reserve is sized so that prompt still fits.
pub const RESERVE_PROMPT_TOKENS: usize = 2_400;

/// Fraction of a chunk's budget re-used as leading overlap in the next chunk.
pub const OVERLAP_FRACTION: f64 = 0.08;

/// One chunk of transcript: whole segments, with the real time span they cover.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// 0-based position in the chunk list.
    pub index: usize,
    /// Start time of the chunk's first segment, in seconds.
    pub start: f64,
    /// End time of the chunk's last segment, in seconds.
    pub end: f64,
    /// The chunk's text: its segments' text, one per line, each prefixed with its start time
    /// so the model can attribute a topic to a time even in the map stage.
    pub text: String,
}

/// Estimated token count of `text` (see the module docs — an estimate, not a tokenization).
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
}

/// How many characters of transcript fit in one chunk given the context window and the
/// output we intend to ask for. Never returns 0 (a nonsense budget yields a small positive
/// one, so chunking always terminates).
pub fn chunk_budget_chars(ctx_tokens: usize, max_output_tokens: usize) -> usize {
    let usable = ctx_tokens
        .saturating_sub(RESERVE_PROMPT_TOKENS)
        .saturating_sub(max_output_tokens)
        .max(256);
    (usable as f64 * CHARS_PER_TOKEN) as usize
}

/// Split `segments` into overlapping chunks of at most `budget_chars` characters.
///
/// Segments are never split. Returns an empty vec for an empty transcript.
pub fn chunk_segments(segments: &[TranscriptSegment], budget_chars: usize) -> Vec<Chunk> {
    if segments.is_empty() {
        return Vec::new();
    }
    let budget = budget_chars.max(256);
    let overlap_budget = (budget as f64 * OVERLAP_FRACTION) as usize;

    // Pre-render each segment's line once; `lines[i].len()` is the cost of segment i.
    let lines: Vec<String> = segments.iter().map(render_line).collect();

    let mut chunks = Vec::new();
    let mut start_idx = 0usize;
    while start_idx < segments.len() {
        let mut end_idx = start_idx; // exclusive
        let mut used = 0usize;
        while end_idx < segments.len() {
            let cost = lines[end_idx].len() + 1;
            // Always take at least one segment, even if it alone blows the budget.
            if used + cost > budget && end_idx > start_idx {
                break;
            }
            used += cost;
            end_idx += 1;
        }

        chunks.push(Chunk {
            index: chunks.len(),
            start: segments[start_idx].start,
            end: segments[end_idx - 1].end,
            text: lines[start_idx..end_idx].join("\n"),
        });

        if end_idx >= segments.len() {
            break;
        }
        // Next chunk re-opens on whole segments worth up to `overlap_budget` characters,
        // but must make progress: it always starts after the current chunk's first segment.
        let mut next_start = end_idx;
        let mut back = 0usize;
        while next_start > start_idx + 1 {
            let cost = lines[next_start - 1].len() + 1;
            if back + cost > overlap_budget {
                break;
            }
            back += cost;
            next_start -= 1;
        }
        start_idx = next_start;
    }
    chunks
}

/// `<start> <text>` — one transcript segment as the model sees it.
fn render_line(seg: &TranscriptSegment) -> String {
    format!("[{:.1}] {}", seg.start, seg.text.trim())
}

/// Join the map stage's per-chunk digests into the reduce stage's body.
pub fn format_digests(digests: &[(Chunk, crate::parse::ChunkDigest)]) -> String {
    let mut out = String::new();
    for (chunk, digest) in digests {
        out.push_str(&format!(
            "--- Part {} ({} to {})\n",
            chunk.index + 1,
            crate::prompt::timecode(chunk.start),
            crate::prompt::timecode(chunk.end)
        ));
        if !digest.summary.trim().is_empty() {
            out.push_str(digest.summary.trim());
            out.push('\n');
        }
        for bullet in &digest.bullets {
            if !bullet.trim().is_empty() {
                out.push_str(&format!("- {}\n", bullet.trim()));
            }
        }
        if !digest.topics.is_empty() {
            out.push_str(&format!("topics: {}\n", digest.topics.join(", ")));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` segments of ~`words` words each, one every 6 seconds.
    fn transcript(n: usize, words: usize) -> Vec<TranscriptSegment> {
        (0..n)
            .map(|i| TranscriptSegment {
                text: vec!["word"; words].join(" "),
                start: i as f64 * 6.0,
                end: i as f64 * 6.0 + 5.5,
            })
            .collect()
    }

    #[test]
    fn budget_shrinks_with_the_output_we_ask_for() {
        let small = chunk_budget_chars(8192, 1024);
        let large = chunk_budget_chars(32_768, 1024);
        assert!(large > small);
        // 8k ctx − 2.4k prompt − 1k output ≈ 4.8k tokens ≈ 16.7k chars.
        assert!((16_000..=17_500).contains(&small), "{small}");
        // A nonsense window still yields a usable, positive budget.
        assert!(chunk_budget_chars(64, 4096) >= 256);
    }

    #[test]
    fn a_two_hour_episode_becomes_several_ordered_overlapping_chunks() {
        // ~20k words over 2 h: 1200 segments × 17 words, one every 6 s.
        let segs = transcript(1200, 17);
        let budget = chunk_budget_chars(8192, 1024);
        let chunks = chunk_segments(&segs, budget);

        assert!(
            chunks.len() >= 5,
            "a 2-hour episode must not fit in {} chunk(s)",
            chunks.len()
        );
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index, i, "chunks are numbered in order");
            assert!(c.end >= c.start);
            // Budget is respected (the only exception is a single oversized segment, which
            // this fixture doesn't have).
            assert!(
                c.text.len() <= budget,
                "chunk {i} is {} chars, budget {budget}",
                c.text.len()
            );
            assert!(!c.text.is_empty());
        }
        // Time-ordered, and overlapping (each chunk re-opens before the previous one ended).
        for pair in chunks.windows(2) {
            assert!(pair[1].start > pair[0].start, "chunks must advance");
            assert!(
                pair[1].start < pair[0].end,
                "chunks must overlap: {} .. {}",
                pair[0].end,
                pair[1].start
            );
        }
        // The whole episode is covered.
        assert_eq!(chunks[0].start, segs[0].start);
        assert_eq!(chunks.last().unwrap().end, segs.last().unwrap().end);
    }

    #[test]
    fn a_short_episode_is_one_chunk() {
        let segs = transcript(20, 12);
        let chunks = chunk_segments(&segs, chunk_budget_chars(8192, 1024));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start, 0.0);
        assert!(
            chunks[0].text.contains("[114.0]"),
            "last segment is present"
        );
    }

    #[test]
    fn an_oversized_segment_becomes_its_own_chunk_rather_than_being_cut() {
        let mut segs = transcript(3, 5);
        segs[1].text = vec!["word"; 5_000].join(" "); // far past any budget
        let chunks = chunk_segments(&segs, 500);
        assert!(chunks.len() >= 2);
        // The monster segment is alone in its chunk, intact.
        let monster = chunks
            .iter()
            .find(|c| c.text.len() > 500)
            .expect("oversized chunk exists");
        assert_eq!(monster.text.lines().count(), 1);
        assert!(monster.text.contains(&vec!["word"; 5_000].join(" ")));
    }

    #[test]
    fn chunking_terminates_and_covers_everything_at_a_silly_budget() {
        let segs = transcript(50, 30);
        let chunks = chunk_segments(&segs, 1);
        assert!(!chunks.is_empty());
        assert_eq!(chunks.last().unwrap().end, segs.last().unwrap().end);
        assert!(chunks.len() <= segs.len(), "no runaway");
    }

    #[test]
    fn empty_transcript_yields_no_chunks() {
        assert!(chunk_segments(&[], 4096).is_empty());
    }

    #[test]
    fn estimate_is_pessimistic() {
        // 350 chars → 100 tokens at 3.5 chars/token; a real tokenizer would say ~85.
        assert_eq!(estimate_tokens(&"a".repeat(350)), 100);
        assert_eq!(estimate_tokens(""), 0);
    }
}
