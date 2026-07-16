//! # anvil-asr
//!
//! Local speech recognition (ADR-004): whisper.cpp via a `whisper-cli` **sidecar process**
//! (not `whisper-rs` — no bindgen/MSVC link step), producing **word-level timestamps** and
//! language auto-detect. Mirrors [`anvil_media`]'s ffmpeg-sidecar pattern; lands in M3.
//!
//! This crate fixes the transcript data model consumed downstream by the cut engine,
//! diarization, and shownotes. The JSON shape below (serde, `snake_case`, times in **seconds**)
//! is a contract those consumers depend on — do not reshape it lightly.
//!
//! ## At a glance
//! - [`transcribe`] / [`WhisperSidecar`] — run whisper.cpp, get a [`Transcript`].
//! - [`TranscribeOptions`] / [`Language`] — model, language, threads.
//! - [`parse_whisper_json`] — the pure (whisper-free) JSON → [`Transcript`] parser.
//! - [`installed_models`] / [`locate_model`] / [`known_models`] — the ggml model manager.
//! - [`diarize`] / [`DiarizeSidecar`] — who spoke when, as a [`Diarization`].
//! - [`assign_speakers`] — merge a [`Diarization`] onto a [`Transcript`]'s word stream.
//!
//! ```no_run
//! use std::path::Path;
//! use anvil_asr::{assign_speakers, diarize, transcribe, DiarizeOptions, TranscribeOptions};
//!
//! // `audio` should be a file whisper reads (ideally 16 kHz mono WAV).
//! let mut transcript = transcribe(Path::new("episode.wav"), &TranscribeOptions::default())?;
//! println!("{} words in {}", transcript.words.len(), transcript.language);
//!
//! // Who spoke when, then stamp every word with its speaker.
//! let speakers = diarize(Path::new("episode.wav"), &DiarizeOptions::default())?;
//! assign_speakers(&mut transcript, &speakers);
//! # Ok::<(), anvil_asr::AsrError>(())
//! ```

use serde::{Deserialize, Serialize};

pub mod diarize;
pub mod error;
pub mod model;
pub mod pin;
pub mod sidecar;

pub use diarize::{
    assign_speakers, diarize, parse_diarization_output, Diarization, DiarizeOptions,
    DiarizeSidecar, Speaker, SpeakerSegment,
};
pub use error::AsrError;
pub use model::{
    installed_diarization_models, installed_models, known_diarization_models, known_models,
    locate_diarization_model, locate_model, models_dirs, verify_model, DiarModelKind,
    DiarModelPack, InstalledModel, ModelPack, KNOWN_DIARIZATION_MODELS, KNOWN_MODELS,
};
pub use pin::{
    macho_content_sha256, sherpa_content_pinned_sha256, sherpa_pin, sherpa_pinned_sha256,
    whisper_content_pinned_sha256, whisper_pin, whisper_pinned_sha256, SidecarPin, SHERPA_PINS,
    WHISPER_PINS,
};
pub use sidecar::{parse_whisper_json, transcribe, Language, TranscribeOptions, WhisperSidecar};

/// One recognized word with its span in the audio, in **seconds**.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// The word text, trimmed (no leading/trailing whitespace). May carry attached
    /// punctuation, e.g. `"world."`.
    pub text: String,
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
    /// Mean whisper token probability for this word, in `[0, 1]` (`0.0` if unscored).
    pub confidence: f32,
    /// Which speaker said this word — a [`Speaker::id`], or `None` if the transcript has not
    /// been diarized (or the word fell outside every speaker turn). Filled in by
    /// [`assign_speakers`]; `#[serde(default)]` so transcripts written before diarization
    /// existed still deserialize.
    #[serde(default)]
    pub speaker: Option<u32>,
}

/// One transcript segment (a sentence-ish grouping of words) with start/end times in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// The segment's text (its words joined by single spaces).
    pub text: String,
    /// Start time in seconds (start of the first word).
    pub start: f64,
    /// End time in seconds (end of the last word).
    pub end: f64,
    /// The segment's dominant speaker — the [`Speaker::id`] holding the most word-time in the
    /// segment, or `None` if undiarized. Filled in by [`assign_speakers`].
    #[serde(default)]
    pub speaker: Option<u32>,
}

/// A full transcript: detected language, the flat word stream, and sentence-ish segments.
///
/// `words` and `segments` cover the same audio at two granularities: `words` is the
/// authoritative per-word stream the cut engine edits against; `segments` is a readable
/// grouping (derived from the words) for display and shownotes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// BCP-47-ish language code (e.g. `"en"`, `"de"`, `"ja"`), or empty if unknown.
    pub language: String,
    /// Every recognized word, in order.
    pub words: Vec<Word>,
    /// Sentence-ish segments, in order.
    pub segments: Vec<Segment>,
}

impl Transcript {
    /// Concatenated plain text of all segments.
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.trim())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_joins_segments() {
        let t = Transcript {
            language: "en".into(),
            words: vec![],
            segments: vec![
                Segment {
                    start: 0.0,
                    end: 1.0,
                    text: "hello".into(),
                    speaker: None,
                },
                Segment {
                    start: 1.0,
                    end: 2.0,
                    text: "world".into(),
                    speaker: None,
                },
            ],
        };
        assert_eq!(t.text(), "hello world");
    }

    #[test]
    fn transcript_json_round_trips_in_contract_shape() {
        let t = Transcript {
            language: "en".into(),
            words: vec![Word {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                confidence: 0.9,
                speaker: Some(0),
            }],
            segments: vec![Segment {
                text: "hi".into(),
                start: 0.0,
                end: 0.4,
                speaker: Some(0),
            }],
        };
        let json = serde_json::to_string(&t).expect("serialize");
        // snake_case field names are part of the contract the UI/cut engine depend on.
        assert!(json.contains("\"language\""));
        assert!(json.contains("\"words\""));
        assert!(json.contains("\"segments\""));
        assert!(json.contains("\"confidence\""));
        assert!(json.contains("\"speaker\""));
        let back: Transcript = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }

    /// The `speaker` field is **additive**: JSON written before diarization existed (no
    /// `speaker` key at all) must still deserialize, landing as `None`. This is the
    /// compatibility guarantee `anvil-cut` / `anvil-cli` / the desktop app rely on.
    #[test]
    fn pre_diarization_json_still_deserializes() {
        let legacy = r#"{
          "language": "en",
          "words": [ { "text": "hi", "start": 0.0, "end": 0.4, "confidence": 0.9 } ],
          "segments": [ { "text": "hi", "start": 0.0, "end": 0.4 } ]
        }"#;
        let t: Transcript = serde_json::from_str(legacy).expect("legacy transcript deserializes");
        assert_eq!(t.words[0].speaker, None);
        assert_eq!(t.segments[0].speaker, None);
        assert_eq!(t.words[0].text, "hi");
        assert_eq!(t.words[0].confidence, 0.9);
    }
}
