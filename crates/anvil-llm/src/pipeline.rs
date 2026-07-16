//! The map-reduce generation pipeline.
//!
//! ```text
//!   transcript ──chunk──▶ [chunk 1] ──map──▶ digest 1 ─┐
//!                         [chunk 2] ──map──▶ digest 2 ─┼──reduce──▶ Shownotes ──sanitize──▶
//!                         [chunk N] ──map──▶ digest N ─┘
//! ```
//!
//! - **Chunk** ([`crate::chunk`]): whole segments, overlapping, sized to the context window.
//! - **Map**: one `llama-cli` call per chunk → a JSON digest (summary / bullets / topics).
//!   Skipped entirely when the episode is one chunk — a 20-minute show is summarized from its
//!   own transcript, not from a summary of it.
//! - **Reduce**: one call over the digests (or the raw transcript) plus the list of legal
//!   chapter timestamps → the final [`Shownotes`] JSON.
//! - **Sanitize** ([`crate::parse::sanitize_chapters`]): whatever the model did with the
//!   timestamps, the chapters that come out are snapped to segment boundaries, strictly
//!   increasing, and inside the episode.
//!
//! A 2-hour episode therefore costs `chunks + 1` generations (typically 7–8) rather than one
//! impossible 26k-token prompt.
//!
//! The pipeline is written against the [`Completer`] trait, not against the sidecar, so
//! [`generate_with`] can be driven by a scripted completer in tests — the map-reduce, the
//! parsing and the snapping are all covered without a 4.7 GB model on the machine.

use std::path::PathBuf;

use crate::chunk::{chunk_budget_chars, chunk_segments, format_digests, Chunk};
use crate::error::LlmError;
use crate::parse::{parse_digest, parse_shownotes, ChapterRules, ChunkDigest};
use crate::prompt::{chapter_candidates, map_prompt, reduce_prompt, ReduceContext};
use crate::sidecar::{Completer, LlamaSidecar};
use crate::{Shownotes, TranscriptInput};

/// Chapter-candidate lines offered to the model (see [`chapter_candidates`]).
const MAX_CANDIDATE_LINES: usize = 80;

/// Everything a generation run needs. [`Default`] is the shipped configuration.
#[derive(Debug, Clone)]
pub struct GenerateOptions {
    /// Explicit gguf path. `None` → `ANVIL_LLM_MODEL`, then the first installed pack.
    pub model: Option<PathBuf>,
    /// Context window to run with. The default is deliberately well under Qwen2.5's 32k: the
    /// KV cache is what puts an 8 GB machine into swap, and the chunker adapts to whatever
    /// this is.
    pub ctx_tokens: usize,
    /// Token budget for each generation.
    pub max_output_tokens: usize,
    /// Sampling temperature. Low: this is an extraction task, not a poem.
    pub temperature: f32,
    /// Nucleus sampling cutoff.
    pub top_p: f32,
    /// RNG seed — fixed by default so the same episode gives the same show notes twice
    /// (users re-run the button; eval runs must be reproducible).
    pub seed: u32,
    /// llama.cpp thread count (`None` = its default).
    pub threads: Option<usize>,
    /// Layers to offload to the GPU (`None` = CPU only, the safe default; ADR-004's
    /// capability probe decides this in the app).
    pub gpu_layers: Option<u32>,
    /// Upper bound on chapters.
    pub max_chapters: usize,
    /// Chapters closer together than this are collapsed.
    pub min_chapter_secs: f64,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            model: None,
            ctx_tokens: 8_192,
            max_output_tokens: 1_024,
            temperature: 0.3,
            top_p: 0.9,
            seed: 0,
            threads: None,
            gpu_layers: None,
            max_chapters: 10,
            min_chapter_secs: 60.0,
        }
    }
}

/// Generate show notes with the local model. Requires the `llama-cli` sidecar **and** an
/// installed gguf; use [`crate::suggest`] for the degrade-gracefully version.
pub fn generate(input: &TranscriptInput, opts: &GenerateOptions) -> Result<Shownotes, LlmError> {
    let sidecar = LlamaSidecar::locate()?;
    generate_with(&sidecar, input, opts)
}

/// [`generate`] against any [`Completer`].
pub fn generate_with(
    completer: &impl Completer,
    input: &TranscriptInput,
    opts: &GenerateOptions,
) -> Result<Shownotes, LlmError> {
    if input.segments.is_empty() {
        return Ok(Shownotes::default());
    }

    let budget = chunk_budget_chars(opts.ctx_tokens, opts.max_output_tokens);
    let chunks = chunk_segments(&input.segments, budget);

    let body = if chunks.len() <= 1 {
        // Short episode: the reduce stage reads the transcript itself. Summarizing a summary
        // when the whole thing fits would only lose detail.
        chunks.first().map(|c| c.text.clone()).unwrap_or_default()
    } else {
        tracing::info!(chunks = chunks.len(), "map stage");
        let digests = map(completer, &chunks, opts)?;
        format_digests(&digests)
    };

    let ctx = ReduceContext {
        language: input.language.clone(),
        duration_secs: input.duration_secs(),
        first_start_secs: input.segments[0].start,
        max_chapters: opts.max_chapters,
        candidates: chapter_candidates(&input.segments, MAX_CANDIDATE_LINES),
    };
    tracing::info!("reduce stage");
    let raw = completer.complete(&reduce_prompt(&body, &ctx), opts)?;

    let rules = ChapterRules::from_segments(
        &input.segments,
        opts.min_chapter_secs,
        opts.max_chapters.max(1),
    );
    parse_shownotes(&raw, &rules)
}

/// The map stage: digest every chunk.
///
/// A chunk whose generation fails or comes back unparseable is **skipped with a warning**,
/// not fatal — losing 12 minutes of a 2-hour episode's detail is a far better outcome than
/// losing the whole job. If *every* chunk fails, the error is propagated.
fn map(
    completer: &impl Completer,
    chunks: &[Chunk],
    opts: &GenerateOptions,
) -> Result<Vec<(Chunk, ChunkDigest)>, LlmError> {
    let mut digests = Vec::with_capacity(chunks.len());
    let mut last_err = None;
    for chunk in chunks {
        let prompt = map_prompt(chunk, chunks.len());
        match completer
            .complete(&prompt, opts)
            .and_then(|raw| parse_digest(&raw))
        {
            Ok(digest) => digests.push((chunk.clone(), digest)),
            Err(err) => {
                tracing::warn!(chunk = chunk.index, %err, "chunk digest failed; skipping it");
                last_err = Some(err);
            }
        }
    }
    match last_err {
        Some(err) if digests.is_empty() => Err(err),
        _ => Ok(digests),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rubric::{self, RubricContext};
    use crate::TranscriptSegment;
    use std::cell::RefCell;

    /// A completer that answers like a well-behaved Qwen2.5 would: a digest for a map prompt,
    /// show notes for a reduce prompt. Records every prompt it saw.
    #[derive(Default)]
    struct ScriptedModel {
        prompts: RefCell<Vec<String>>,
        /// Chatter the model wraps its JSON in, to prove the parser copes end to end.
        messy: bool,
    }

    impl Completer for ScriptedModel {
        fn complete(&self, prompt: &str, _opts: &GenerateOptions) -> Result<String, LlmError> {
            self.prompts.borrow_mut().push(prompt.to_string());
            let json = if prompt.contains("CHAPTER CANDIDATES") {
                // Reduce: note the deliberately awful chapter times — mid-segment, a clock
                // string, and one past the end of the episode.
                r#"{
                  "summary": "Two engineers spend an episode arguing about how to record a podcast in a room that fights back, why a dynamic microphone forgives an untreated space, and how to cut filler words without making the conversation sound clipped.",
                  "bullets": ["Dynamic mics forgive a bad room.", "Treat the first reflection.",
                              "Cut filler words in the transcript view."],
                  "chapters": [
                    {"title": "Microphones", "start": 7.5},
                    {"title": "The room", "start": "10:03"},
                    {"title": "Editing", "start": 1207.0},
                    {"title": "Outro", "start": 999999.0}
                  ],
                  "titles": ["Recording in a bad room", "Mics, rooms and edits"],
                  "keywords": ["microphones", "acoustics", "editing", "podcasting", "reverb"]
                }"#
                .to_string()
            } else {
                r#"{"summary": "This part covers gear.",
                    "bullets": ["They compare two microphones."],
                    "topics": ["microphones"]}"#
                    .to_string()
            };
            Ok(if self.messy {
                format!("Sure! Here you go:\n```json\n{json}\n```\n[end of text]")
            } else {
                json
            })
        }
    }

    /// `n` segments of 20 words, one every 6 seconds — 1200 of them is a 2-hour episode.
    fn episode(n: usize) -> TranscriptInput {
        TranscriptInput {
            language: "en".into(),
            segments: (0..n)
                .map(|i| TranscriptSegment {
                    text: format!(
                        "segment {i} in which the hosts discuss microphones rooms and editing \
                         software at some length with several words"
                    ),
                    start: i as f64 * 6.0,
                    end: i as f64 * 6.0 + 5.5,
                })
                .collect(),
        }
    }

    #[test]
    fn a_two_hour_episode_is_mapped_then_reduced() {
        let model = ScriptedModel {
            messy: true,
            ..Default::default()
        };
        let input = episode(1200);
        let notes = generate_with(&model, &input, &GenerateOptions::default()).expect("generate");

        let prompts = model.prompts.borrow();
        let maps = prompts
            .iter()
            .filter(|p| p.contains("Write a digest of this part only"))
            .count();
        let reduces = prompts
            .iter()
            .filter(|p| p.contains("CHAPTER CANDIDATES"))
            .count();
        assert!(maps >= 5, "one map call per chunk, got {maps}");
        assert_eq!(reduces, 1, "exactly one reduce call");
        assert_eq!(prompts.len(), maps + reduces);

        // Every prompt is ChatML and none of them overflows the context window.
        for p in prompts.iter() {
            assert!(p.starts_with("<|im_start|>system\n"));
            let tokens = crate::chunk::estimate_tokens(p);
            assert!(tokens < 8_192 - 1_024, "prompt is {tokens} tokens");
        }
        // The reduce prompt was fed the map stage's digests, not the raw transcript.
        let reduce = prompts
            .iter()
            .find(|p| p.contains("CHAPTER CANDIDATES"))
            .unwrap();
        assert!(reduce.contains("This part covers gear."));
        assert!(reduce.contains("--- Part 1 (0:00 to"));

        // And the model's mangled chapters came out clean.
        let starts: Vec<f64> = notes.chapters.iter().map(|c| c.start).collect();
        assert_eq!(starts, [0.0, 600.0, 1206.0], "snapped, sorted, clamped");
        assert!(notes.chapters.iter().all(|c| !c.title.is_empty()));
        assert!(notes.summary.starts_with("Two engineers spend an episode"));
        assert_eq!(notes.bullets.len(), 3);
        assert_eq!(notes.keywords.len(), 5);
    }

    #[test]
    fn a_short_episode_skips_the_map_stage() {
        let model = ScriptedModel::default();
        let input = episode(40); // 4 minutes: one chunk
        let notes = generate_with(&model, &input, &GenerateOptions::default()).expect("generate");

        let prompts = model.prompts.borrow();
        assert_eq!(prompts.len(), 1, "reduce only");
        assert!(prompts[0].contains("CHAPTER CANDIDATES"));
        // The reduce prompt saw the actual transcript.
        assert!(prompts[0].contains("segment 39 in which the hosts"));
        assert!(!notes.chapters.is_empty());
    }

    #[test]
    fn the_result_passes_the_usability_rubric() {
        let model = ScriptedModel::default();
        let input = episode(600);
        let notes = generate_with(&model, &input, &GenerateOptions::default()).expect("generate");
        let report = rubric::score(&notes, &RubricContext::from_input(&input));
        assert!(report.usable, "{report:#?}");
    }

    #[test]
    fn one_bad_chunk_does_not_sink_the_job() {
        /// Fails the first map call, behaves after that.
        struct Flaky {
            inner: ScriptedModel,
            calls: RefCell<usize>,
        }
        impl Completer for Flaky {
            fn complete(&self, prompt: &str, opts: &GenerateOptions) -> Result<String, LlmError> {
                let mut calls = self.calls.borrow_mut();
                *calls += 1;
                if *calls == 1 {
                    return Err(LlmError::SidecarFailed("out of memory".into()));
                }
                self.inner.complete(prompt, opts)
            }
        }
        let model = Flaky {
            inner: ScriptedModel::default(),
            calls: RefCell::new(0),
        };
        let notes =
            generate_with(&model, &episode(1200), &GenerateOptions::default()).expect("generate");
        assert!(!notes.chapters.is_empty());
        assert!(!notes.summary.is_empty());
    }

    #[test]
    fn a_model_that_only_talks_prose_is_an_error_not_a_panic() {
        struct Chatty;
        impl Completer for Chatty {
            fn complete(&self, _p: &str, _o: &GenerateOptions) -> Result<String, LlmError> {
                Ok("I'm afraid I can't do that, Dave.".into())
            }
        }
        let err = generate_with(&Chatty, &episode(40), &GenerateOptions::default())
            .expect_err("must fail");
        assert!(matches!(err, LlmError::NoJson(_)));
    }

    #[test]
    fn an_empty_transcript_generates_nothing_without_calling_the_model() {
        let model = ScriptedModel::default();
        let notes = generate_with(
            &model,
            &TranscriptInput::default(),
            &GenerateOptions::default(),
        )
        .expect("generate");
        assert_eq!(notes, Shownotes::default());
        assert!(model.prompts.borrow().is_empty(), "no model call at all");
    }
}
