//! Shownotes (04 §S2 Chapters & Metadata tab "AI suggest", M4): generate an episode summary,
//! chapter markers, candidate titles, and keywords from the last `transcribe` result via
//! `anvil_llm`.
//!
//! ## Two engines, one shape out — honestly labelled
//! `anvil_llm` ships two paths: [`anvil_llm::generate`] (the good one — a local Qwen2.5 LLM via
//! the `llama-cli` sidecar) and [`anvil_llm::fallback::shownotes`] (a no-model extractive
//! summary + topic-drift chapters + TF-IDF keywords). This command tries the LLM first, and on
//! *any* failure — the sidecar isn't installed, no Qwen gguf is present, or a run fails — falls
//! back to the extractive path and **says so** in [`ShownotesResult::engine`] +
//! [`ShownotesResult::note`], rather than erroring. A missing multi-GB model is a quality
//! downgrade, not a dead end (see `anvil_llm`'s crate docs), and the fallback is a real result,
//! never a fabricated one. That is also why this feature is genuinely wired end-to-end today:
//! the fallback path needs no sidecar at all.
//!
//! The model, when installed, is resolved the same way `transcript::transcribe` resolves the
//! whisper weights — including the Models-screen download dir ([`ModelsState::dir`]), which
//! `anvil_llm`'s own search path doesn't know about — so a Qwen pack downloaded from the Models
//! screen is actually used.

use std::path::{Path, PathBuf};

use anvil_llm::{FallbackOptions, GenerateOptions, LlmError, TranscriptInput};
use serde::Serialize;
use tauri::State;

use crate::models::ModelsState;
use crate::transcript::{Transcript, TranscriptState};

/// One suggested chapter marker (04 §S2 "chapter list"). `start_secs` always lands on a real
/// transcript segment boundary — see [`anvil_llm::Chapter`].
#[derive(Debug, Clone, Serialize)]
pub struct ShownoteChapter {
    pub title: String,
    pub start_secs: f64,
}

/// The generated show notes, plus which engine produced them so the UI can be honest.
#[derive(Debug, Clone, Serialize)]
pub struct ShownotesResult {
    pub summary: String,
    pub bullets: Vec<String>,
    pub chapters: Vec<ShownoteChapter>,
    pub titles: Vec<String>,
    pub keywords: Vec<String>,
    /// `"llm"` when the local Qwen model produced these, `"fallback"` when the built-in
    /// extractive summarizer did (no model installed / the sidecar couldn't run).
    pub engine: &'static str,
    /// Present on the fallback path: a one-line, actionable explanation of why the AI model
    /// wasn't used, safe to show the user.
    pub note: Option<String>,
}

/// Generate show notes from the last `transcribe` result (04 §S2 "AI suggest").
///
/// Requires a transcript first (the notes are made *from* it). Uses the local Qwen model when
/// it and the `llama-cli` sidecar are installed; otherwise degrades cleanly to the built-in
/// extractive summarizer, flagged in the result — never an error dialog, never a crash.
#[tauri::command]
pub fn generate_shownotes(
    tstate: State<'_, TranscriptState>,
    mstate: State<'_, ModelsState>,
) -> Result<ShownotesResult, String> {
    let transcript = tstate
        .snapshot()
        .ok_or_else(|| "transcribe the file before generating shownotes".to_string())?;
    if transcript.segments.is_empty() {
        return Err(
            "this transcript has no sentences to summarize — transcribe the file first".to_string(),
        );
    }

    let input = to_llm_input(&transcript);
    let opts = GenerateOptions {
        model: resolve_shownotes_model(mstate.dir()),
        ..GenerateOptions::default()
    };

    let (notes, engine, note) = match anvil_llm::generate(&input, &opts) {
        Ok(notes) => (notes, "llm", None),
        Err(err) => {
            tracing::info!(%err, "local shownotes LLM unavailable; using the extractive fallback");
            let fallback_opts = FallbackOptions {
                max_chapters: opts.max_chapters,
                min_chapter_secs: opts.min_chapter_secs,
                ..FallbackOptions::default()
            };
            let notes = anvil_llm::fallback::shownotes(&input, &fallback_opts);
            (notes, "fallback", Some(fallback_note(&err)))
        }
    };

    Ok(ShownotesResult {
        summary: notes.summary,
        bullets: notes.bullets,
        chapters: notes
            .chapters
            .into_iter()
            .map(|c| ShownoteChapter {
                title: c.title,
                start_secs: c.start,
            })
            .collect(),
        titles: notes.titles,
        keywords: notes.keywords,
        engine,
        note,
    })
}

/// Convert the wire transcript into `anvil_llm`'s input (segments only — chapters snap to
/// segment boundaries, so word-level timings aren't needed here).
fn to_llm_input(t: &Transcript) -> TranscriptInput {
    TranscriptInput {
        language: t.language.clone(),
        segments: t
            .segments
            .iter()
            .map(|s| anvil_llm::TranscriptSegment {
                text: s.text.clone(),
                start: s.start,
                end: s.end,
            })
            .collect(),
    }
}

/// Resolve an installed Qwen gguf: `anvil_llm`'s own installed packs first, then the
/// Models-screen download dir (which `anvil_llm`'s search path doesn't cover). `None` lets
/// [`anvil_llm::generate`] fall back to its own `ANVIL_LLM_MODEL`/env resolution before
/// finally erroring into the extractive path.
fn resolve_shownotes_model(models_dir: &Path) -> Option<PathBuf> {
    if let Some(installed) = anvil_llm::installed_models().into_iter().next() {
        return Some(installed.path);
    }
    for pack in anvil_llm::known_models() {
        let candidate = models_dir.join(pack.primary_filename());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A friendly, actionable note explaining why the fallback engine was used.
fn fallback_note(err: &LlmError) -> String {
    match err {
        LlmError::SidecarNotFound(_) | LlmError::ModelNotFound(_) => {
            "Generated with the built-in summarizer. Install the Qwen shownotes model in Models \
             for AI-written notes."
                .to_string()
        }
        other => {
            format!("Generated with the built-in summarizer — the AI model couldn't run ({other}).")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::TranscriptSegment;

    fn transcript_with_segments(n: usize) -> Transcript {
        Transcript {
            language: "en".into(),
            words: vec![],
            segments: (0..n)
                .map(|i| TranscriptSegment {
                    text: format!(
                        "in part {i} the hosts discuss microphone technique and room acoustics \
                         at some reasonable length for a podcast episode"
                    ),
                    start: i as f64 * 40.0,
                    end: i as f64 * 40.0 + 38.0,
                    speaker: None,
                })
                .collect(),
        }
    }

    #[test]
    fn to_llm_input_carries_language_and_segments() {
        let input = to_llm_input(&transcript_with_segments(3));
        assert_eq!(input.language, "en");
        assert_eq!(input.segments.len(), 3);
        assert!(input.duration_secs() > 0.0);
    }

    #[test]
    fn fallback_note_is_actionable_for_a_missing_model() {
        let note = fallback_note(&LlmError::ModelNotFound("no gguf".into()));
        assert!(note.contains("Models"));
        let note = fallback_note(&LlmError::SidecarNotFound("no llama-cli".into()));
        assert!(note.contains("Models"));
    }

    /// The fallback engine is real and needs no model — the thing that makes this feature
    /// wired end-to-end even without the multi-GB Qwen download.
    #[test]
    fn the_extractive_fallback_produces_usable_notes_with_no_model() {
        let input = to_llm_input(&transcript_with_segments(30));
        let opts = FallbackOptions::default();
        let notes = anvil_llm::fallback::shownotes(&input, &opts);
        assert!(!notes.summary.is_empty());
        assert!(!notes.chapters.is_empty());
        assert!(!notes.keywords.is_empty());
        assert_eq!(notes.chapters[0].start, 0.0);
    }
}
