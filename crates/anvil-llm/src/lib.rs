//! # anvil-llm
//!
//! Local shownotes, chapters, titles and tags (ADR-004, M4 lane D). 100% on-device: a
//! **llama.cpp sidecar** running **Qwen2.5-Instruct** (7B default, 1.5B low-RAM — both
//! Apache-2.0; never the research-licensed 3B). The transcript never leaves the machine,
//! there is no API key, and nothing is fetched at inference time.
//!
//! ## Two ways in, one shape out
//! The model pack is a 1–4.7 GB optional download, so **everything degrades**:
//!
//! | | needs the pack | quality |
//! |---|---|---|
//! | [`generate`] | yes | the good one: written summary, real chapter titles |
//! | [`fallback::shownotes`] | **no** | extractive summary, topic-drift chapters, TF-IDF tags |
//! | [`suggest`] | no | `generate`, falling back to the above on any failure |
//!
//! Both paths return the same [`Shownotes`], so the Chapters & Metadata tab (04 §S2) has one
//! code path and the "AI suggest" button works on a fresh install with no model at all.
//!
//! ## Why a sidecar, not `llama-cpp-2`
//! Same reason [`anvil_asr`] shells out to `whisper-cli` and [`anvil_media`] to `ffmpeg`:
//! linking llama.cpp means cmake + bindgen + libclang + a C++ toolchain on every dev machine
//! and CI runner, a multi-minute build, and a GPU-backend matrix baked into *our* binary.
//! Sidecar keeps `cargo build` pure-Rust and seconds long, and lets a user drop in a
//! Vulkan/Metal/CUDA build of llama.cpp without recompiling Cleanroom. See [`sidecar`].
//!
//! ## The pipeline
//! [`chunk`] (fit a 2-hour episode into an 8k window) → [`prompt`] (one versioned place) →
//! [`pipeline`] (map-reduce) → [`parse`] (extract JSON, snap chapters to segment boundaries)
//! → [`rubric`] (is this publishable without a major edit?).
//!
//! ```no_run
//! use anvil_llm::{suggest, GenerateOptions, TranscriptInput};
//!
//! let transcript = anvil_asr::transcribe(
//!     std::path::Path::new("episode.wav"),
//!     &anvil_asr::TranscribeOptions::default(),
//! )?;
//! // Uses the local model if the pack is installed; falls back to topic-drift if not.
//! let notes = suggest(&TranscriptInput::from(&transcript), &GenerateOptions::default());
//! for chapter in &notes.chapters {
//!     println!("{:>8.1}  {}", chapter.start, chapter.title);
//! }
//! # Ok::<(), anvil_asr::AsrError>(())
//! ```

use serde::{Deserialize, Serialize};

pub mod chunk;
pub mod error;
pub mod fallback;
pub mod model;
pub mod parse;
pub mod pipeline;
pub mod prompt;
pub mod rubric;
pub mod sidecar;

pub use error::LlmError;
pub use fallback::FallbackOptions;
pub use model::{
    installed_models, known_models, locate_model, models_dirs, sha256_file, verify_model,
    verify_model_in, InstalledModel, ModelFile, ModelPack, Verification, DEFAULT_MODEL_ID,
    KNOWN_MODELS, LOW_RAM_MODEL_ID,
};
pub use pipeline::{generate, generate_with, GenerateOptions};
pub use prompt::PROMPT_VERSION;
pub use rubric::{RubricContext, RubricReport};
pub use sidecar::{Completer, LlamaSidecar};

/// One transcript segment: a sentence-ish grouping of words, times in **seconds**.
///
/// Mirrors [`anvil_asr::Segment`] — this crate keeps its own copy so a caller can generate
/// show notes from an imported SRT or a hand-typed transcript without inventing word-level
/// timings it does not have.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub text: String,
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
}

/// The input to every function in this crate: an episode's segments and its language.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptInput {
    /// BCP-47-ish language code (`"en"`, `"de"`, …), or empty. Passed to the model so the
    /// show notes come back in the language the episode was actually in.
    pub language: String,
    /// The segments, in time order.
    pub segments: Vec<TranscriptSegment>,
}

impl TranscriptInput {
    /// Episode duration: the end of the last segment (0 if there are none).
    pub fn duration_secs(&self) -> f64 {
        self.segments.last().map(|s| s.end).unwrap_or(0.0)
    }

    /// The transcript as plain text.
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.trim())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The ASR lane's transcript is the contract this lane consumes (words are not needed —
/// chapters snap to segment boundaries).
impl From<&anvil_asr::Transcript> for TranscriptInput {
    fn from(t: &anvil_asr::Transcript) -> Self {
        Self {
            language: t.language.clone(),
            segments: t
                .segments
                .iter()
                .map(|s| TranscriptSegment {
                    text: s.text.clone(),
                    start: s.start,
                    end: s.end,
                })
                .collect(),
        }
    }
}

/// A suggested chapter marker. `start` is in seconds and always lands on a real transcript
/// segment boundary — the chapter writer (ID3v2 CHAP / MP4 atoms) relies on that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    /// Start time in seconds.
    pub start: f64,
}

/// Generated episode metadata (all of it user-editable — these are suggestions, not facts).
///
/// The JSON shape (serde, `snake_case`) is the contract the UI and the CLI's `--json` output
/// depend on:
/// ```json
/// {
///   "summary": "…",
///   "bullets": ["…"],
///   "chapters": [{ "title": "…", "start": 0.0 }],
///   "titles": ["…"],
///   "keywords": ["…"]
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Shownotes {
    /// A few sentences a listener reads before pressing play.
    pub summary: String,
    /// Key points, one sentence each.
    pub bullets: Vec<String>,
    /// Chapter markers, strictly increasing, inside the episode.
    pub chapters: Vec<Chapter>,
    /// Candidate episode titles.
    pub titles: Vec<String>,
    /// Tags/keywords, lowercase.
    pub keywords: Vec<String>,
}

/// Show notes with the best engine available, never failing.
///
/// Tries the local model ([`generate`]); if the sidecar is missing, no model pack is
/// installed, the generation fails, or the model returns something unparseable, it logs why
/// and returns the no-LLM [`fallback::shownotes`] instead. This is what the "AI suggest"
/// button calls: a missing 4.7 GB download is a quality downgrade, not an error dialog.
pub fn suggest(input: &TranscriptInput, opts: &GenerateOptions) -> Shownotes {
    match generate(input, opts) {
        Ok(notes) => notes,
        Err(err) => {
            tracing::info!(%err, "local LLM unavailable; using the topic-drift fallback");
            let fallback_opts = FallbackOptions {
                max_chapters: opts.max_chapters,
                min_chapter_secs: opts.min_chapter_secs,
                ..Default::default()
            };
            fallback::shownotes(input, &fallback_opts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shownotes_round_trip_in_the_contract_shape() {
        let notes = Shownotes {
            summary: "An episode about audio.".into(),
            bullets: vec!["We talked about mics.".into()],
            chapters: vec![Chapter {
                title: "Intro".into(),
                start: 0.0,
            }],
            titles: vec!["Mastering Locally".into()],
            keywords: vec!["audio".into()],
        };
        let json = serde_json::to_string(&notes).expect("serialize");
        // snake_case field names are the contract the UI and `anvil --json` depend on.
        for key in [
            "\"summary\"",
            "\"bullets\"",
            "\"chapters\"",
            "\"titles\"",
            "\"keywords\"",
            "\"title\"",
            "\"start\"",
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        let back: Shownotes = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, notes);
    }

    #[test]
    fn an_asr_transcript_converts_straight_into_the_input() {
        let transcript = anvil_asr::Transcript {
            language: "en".into(),
            words: vec![anvil_asr::Word {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                confidence: 0.9,
                speaker: None,
            }],
            segments: vec![
                anvil_asr::Segment {
                    text: "hi there".into(),
                    start: 0.0,
                    end: 1.0,
                    speaker: None,
                },
                anvil_asr::Segment {
                    text: "welcome back".into(),
                    start: 1.0,
                    end: 2.5,
                    speaker: None,
                },
            ],
        };
        let input = TranscriptInput::from(&transcript);
        assert_eq!(input.language, "en");
        assert_eq!(input.segments.len(), 2);
        assert_eq!(input.duration_secs(), 2.5);
        assert_eq!(input.text(), "hi there welcome back");
    }

    #[test]
    fn suggest_falls_back_to_topic_drift_when_there_is_no_model() {
        // No sidecar + no pack on a CI runner → this must still return usable show notes
        // rather than an error. (If a dev machine *does* have llama-cli and a gguf, the LLM
        // path runs instead and the assertions below still hold.)
        let input = TranscriptInput {
            language: "en".into(),
            segments: (0..30)
                .map(|i| TranscriptSegment {
                    text: format!(
                        "in this part the hosts discuss microphone technique and room \
                         acoustics for episode number {i} at some reasonable length"
                    ),
                    start: i as f64 * 40.0,
                    end: i as f64 * 40.0 + 38.0,
                })
                .collect(),
        };
        let notes = suggest(&input, &GenerateOptions::default());

        assert!(!notes.summary.is_empty());
        assert!(!notes.bullets.is_empty());
        assert!(!notes.chapters.is_empty());
        assert!(!notes.keywords.is_empty());
        assert_eq!(notes.chapters[0].start, 0.0);
        for chapter in &notes.chapters {
            assert!(chapter.start <= input.duration_secs());
        }
    }

    #[test]
    fn an_empty_transcript_is_empty_shownotes_not_a_crash() {
        let notes = suggest(&TranscriptInput::default(), &GenerateOptions::default());
        assert_eq!(notes, Shownotes::default());
    }
}
