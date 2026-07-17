//! whisper.cpp sidecar manager.
//!
//! whisper.cpp is **never linked** into Cleanroom (no `whisper-rs`, no bindgen/MSVC hurdle) — it
//! is run as a separate `whisper-cli` child process, mirroring [`anvil_media::FfmpegSidecar`].
//! This keeps the engine build fast and portable: we only invoke an unmodified whisper.cpp
//! CLI at arm's length.
//!
//! Airplane-mode (ADR-005 engine invariant): this module **never downloads** anything. The
//! binary must already be present — pointed at by `CLEANROOM_WHISPER`, bundled next to the app,
//! or on `PATH` — and the model must already be on disk (see [`crate::model`]).
//!
//! ## Transcription recipe
//! We invoke whisper.cpp for **word-level** timestamps:
//! `-ml 1 -sow` makes every transcription entry a single word, and `-ojf` writes the full
//! JSON (per-token `p` probabilities, which the plain `-oj` output omits but the
//! [`Word::confidence`](crate::Word) field needs). Offsets in that JSON are milliseconds. The
//! parser ([`parse_whisper_json`]) turns each non-empty entry into a [`Word`] and groups the
//! words into sentence-ish [`Segment`]s (see [`derive_segments`]).
//!
//! ## Audio expectations
//! whisper.cpp reads `wav`/`flac`/`mp3`/`ogg` and resamples internally, but is happiest with
//! **16 kHz mono 16-bit WAV**. Callers that hold a decoded [`anvil_media::AudioBuffer`] should
//! write such a WAV (or decode straight to one) and pass its path — this crate deliberately
//! does not depend on `anvil-media` so the ASR lane stays decoupled from the media lane.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::error::AsrError;
use crate::{Segment, Transcript, Word};

/// Words separated by more than this many seconds start a new derived [`Segment`], even
/// without sentence punctuation — this recovers speaker/utterance boundaries in audio the
/// model does not punctuate.
const SEGMENT_GAP_SECS: f64 = 0.8;

/// The spoken language passed to whisper's `-l` flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Language {
    /// `-l auto`: let whisper detect the language (multilingual models only; an `.en` model
    /// always decodes English regardless).
    #[default]
    Auto,
    /// An explicit code, e.g. `"en"`, `"de"`, `"ja"`.
    Code(String),
}

impl Language {
    /// The value for whisper's `-l` argument.
    fn as_arg(&self) -> &str {
        match self {
            Language::Auto => "auto",
            Language::Code(code) => code,
        }
    }
}

/// Knobs for a single [`transcribe`] call.
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    /// Language to decode (default [`Language::Auto`]).
    pub language: Language,
    /// Explicit model `.bin` path. When `None`, the model is resolved from
    /// `CLEANROOM_WHISPER_MODEL`, then the first installed model in the models dir.
    pub model: Option<PathBuf>,
    /// Thread count for whisper's `-t` flag. `None` leaves whisper's default.
    pub threads: Option<usize>,
}

/// A located `whisper-cli` binary, reusable across transcribe calls.
#[derive(Debug, Clone)]
pub struct WhisperSidecar {
    binary: PathBuf,
}

impl WhisperSidecar {
    /// Locate `whisper-cli` without touching the network. Search order:
    /// 1. `CLEANROOM_WHISPER` environment variable (explicit path),
    /// 2. a bundled sidecar next to the current executable (`whisper-cli`, `sidecar/…`,
    ///    `whisper/…`, and — for a macOS `.app`, where the exe is in `Contents/MacOS/` and
    ///    Tauri drops resources in `Contents/Resources/` — `../Resources/whisper/…`),
    /// 3. `whisper-cli` on `PATH`.
    ///
    /// Returns [`AsrError::SidecarNotFound`] if none exist (airplane-mode: we do not download
    /// it).
    pub fn locate() -> Result<Self, AsrError> {
        for candidate in Self::candidates() {
            if candidate.is_file() {
                return Self::from_path(candidate);
            }
        }
        if let Some(found) = Self::search_path() {
            return Self::from_path(found);
        }
        Err(AsrError::SidecarNotFound(
            "no bundled sidecar, CLEANROOM_WHISPER unset, and whisper-cli not on PATH \
             (airplane-mode: Cleanroom never auto-downloads it)"
                .into(),
        ))
    }

    /// Wrap an explicit `whisper-cli` path.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, AsrError> {
        let binary = path.into();
        if !binary.is_file() {
            return Err(AsrError::SidecarNotFound(binary.display().to_string()));
        }
        Ok(Self { binary })
    }

    /// Path of the resolved binary.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    fn exe_name() -> String {
        // `EXE_SUFFIX` is ".exe" on Windows and "" elsewhere — cross-platform without any
        // `#[cfg]` (which the workspace confines to anvil-core::platform).
        format!("whisper-cli{}", std::env::consts::EXE_SUFFIX)
    }

    fn candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(explicit) = std::env::var_os("CLEANROOM_WHISPER") {
            out.push(PathBuf::from(explicit));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let name = Self::exe_name();
                out.push(dir.join(&name));
                out.push(dir.join("sidecar").join(&name));
                out.push(dir.join("whisper").join(&name));
                // macOS `.app` layout: the exe is at `Contents/MacOS/<app>` and Tauri's
                // `bundle.resources` land in `Contents/Resources/`, so the packaged `whisper/`
                // folder is `../Resources/whisper/` relative to the exe. Added unconditionally
                // (no `#[cfg]`, which the workspace confines to anvil-core::platform): this path
                // does not exist in a Windows/flat install, so it never matches there — Windows
                // resolution is byte-identical.
                out.push(dir.join("../Resources/whisper").join(&name));
            }
        }
        out
    }

    fn search_path() -> Option<PathBuf> {
        let name = Self::exe_name();
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(&name))
            .find(|candidate| candidate.is_file())
    }

    /// Transcribe `audio` into a [`Transcript`] with word-level timestamps.
    ///
    /// The model is resolved (in order) from `opts.model`, `CLEANROOM_WHISPER_MODEL`, then the
    /// first installed model in the models dir; a missing model yields
    /// [`AsrError::ModelNotFound`]. `audio` should be a file whisper reads (ideally a 16 kHz
    /// mono WAV — see the module docs).
    pub fn transcribe(
        &self,
        audio: &Path,
        opts: &TranscribeOptions,
    ) -> Result<Transcript, AsrError> {
        let model = resolve_model(opts)?;
        let json = self.run(audio, &model, opts)?;
        parse_whisper_json(&json)
    }

    /// Run `whisper-cli` and return the contents of the JSON file it writes.
    fn run(
        &self,
        audio: &Path,
        model: &Path,
        opts: &TranscribeOptions,
    ) -> Result<String, AsrError> {
        if !audio.is_file() {
            return Err(AsrError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("audio file not found: {}", audio.display()),
            )));
        }

        // whisper appends ".json" to the `-of` base; use a unique temp base so concurrent
        // jobs never collide, and clean it up after reading.
        let out_base = temp_output_base();
        let out_json = out_base.with_extension("json");

        tracing::debug!(
            binary = %self.binary.display(),
            model = %model.display(),
            audio = %audio.display(),
            language = opts.language.as_arg(),
            "running whisper-cli"
        );

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-m")
            .arg(model)
            .arg("-f")
            .arg(audio)
            .args(["-l", opts.language.as_arg()])
            // Word-level output: one word per entry (`-ml 1 -sow`), full JSON for per-token
            // `p` confidence (`-ojf` implies `-oj`), and no console prints (`-np`).
            .args(["-ml", "1", "-sow", "-oj", "-ojf", "-np"])
            .arg("-of")
            .arg(&out_base);
        if let Some(threads) = opts.threads {
            cmd.args(["-t", &threads.to_string()]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output()?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&out_json);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AsrError::SidecarFailed(format!(
                "whisper-cli exited with {}: {}",
                output.status,
                tail(&stderr, 800)
            )));
        }

        let json = std::fs::read_to_string(&out_json).map_err(|e| {
            AsrError::SidecarFailed(format!(
                "whisper-cli reported success but no JSON at {}: {e}",
                out_json.display()
            ))
        })?;
        let _ = std::fs::remove_file(&out_json);
        Ok(json)
    }
}

/// Locate the sidecar and transcribe `audio` in one call. Convenience wrapper over
/// [`WhisperSidecar::locate`] + [`WhisperSidecar::transcribe`].
pub fn transcribe(audio: &Path, opts: &TranscribeOptions) -> Result<Transcript, AsrError> {
    WhisperSidecar::locate()?.transcribe(audio, opts)
}

/// Resolve the model path: explicit `opts.model` wins, then `CLEANROOM_WHISPER_MODEL`, then the
/// first model installed in the models dir. Never downloads.
fn resolve_model(opts: &TranscribeOptions) -> Result<PathBuf, AsrError> {
    if let Some(model) = &opts.model {
        if model.is_file() {
            return Ok(model.clone());
        }
        return Err(AsrError::ModelNotFound(model.display().to_string()));
    }
    if let Some(env) = std::env::var_os("CLEANROOM_WHISPER_MODEL") {
        let path = PathBuf::from(env);
        if path.is_file() {
            return Ok(path);
        }
        return Err(AsrError::ModelNotFound(path.display().to_string()));
    }
    if let Some(installed) = crate::model::installed_models().into_iter().next() {
        return Ok(installed.path);
    }
    Err(AsrError::ModelNotFound(
        "no model given, CLEANROOM_WHISPER_MODEL unset, and no ggml-*.bin in the models dir".into(),
    ))
}

/// A unique `-of` base path in the system temp dir (no extension; whisper adds `.json`).
fn temp_output_base() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut base = std::env::temp_dir();
    base.push(format!("anvil-asr-{}-{nanos}", std::process::id()));
    base
}

/// Last `max` chars of `s`, trimmed — used to keep a whisper stderr tail bounded.
fn tail(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let start = trimmed.len() - max;
    // Snap to a char boundary so we never slice through a multi-byte codepoint.
    let start = (start..trimmed.len())
        .find(|&i| trimmed.is_char_boundary(i))
        .unwrap_or(trimmed.len());
    trimmed[start..].to_string()
}

// --- whisper.cpp JSON shape (only the fields we consume) -------------------------------

#[derive(Deserialize)]
struct WhisperJson {
    #[serde(default)]
    result: WhisperResult,
    #[serde(default)]
    transcription: Vec<WhisperEntry>,
}

#[derive(Deserialize, Default)]
struct WhisperResult {
    #[serde(default)]
    language: String,
}

#[derive(Deserialize)]
struct WhisperEntry {
    #[serde(default)]
    text: String,
    offsets: Offsets,
    #[serde(default)]
    tokens: Vec<WhisperToken>,
}

/// Millisecond offsets from the start of the audio.
#[derive(Deserialize)]
struct Offsets {
    from: i64,
    to: i64,
}

#[derive(Deserialize)]
struct WhisperToken {
    #[serde(default)]
    text: String,
    #[serde(default)]
    p: f32,
}

/// Parse whisper.cpp `-ml 1 -sow -ojf` JSON into a [`Transcript`].
///
/// Runs with **no** whisper process — pure string → struct — so it is fully unit-testable
/// against a captured sample. Each non-empty `transcription` entry becomes a [`Word`] (times
/// converted from ms to seconds); words are then grouped into sentence-ish [`Segment`]s.
pub fn parse_whisper_json(json: &str) -> Result<Transcript, AsrError> {
    let raw: WhisperJson = serde_json::from_str(json)?;

    let mut words = Vec::with_capacity(raw.transcription.len());
    for entry in &raw.transcription {
        let text = entry.text.trim();
        // whisper emits an empty leading `[_BEG_]` entry and can emit blank ones; skip them.
        if text.is_empty() {
            continue;
        }
        words.push(Word {
            text: text.to_string(),
            start: entry.offsets.from as f64 / 1000.0,
            end: entry.offsets.to as f64 / 1000.0,
            confidence: word_confidence(&entry.tokens),
            // whisper has no notion of speakers; `crate::assign_speakers` fills this in from a
            // `Diarization` afterwards.
            speaker: None,
        });
    }

    let segments = derive_segments(&words);
    Ok(Transcript {
        language: raw.result.language,
        words,
        segments,
    })
}

/// Mean probability of a word's real tokens. whisper special tokens (`[_BEG_]`, `[_TT_..]`)
/// carry their own `p` but are not part of the word, so they are excluded. Returns `0.0` when
/// a word has no scorable tokens (e.g. a plain `-oj` sample without the `-ojf` token array).
fn word_confidence(tokens: &[WhisperToken]) -> f32 {
    let scored: Vec<f32> = tokens
        .iter()
        .filter(|t| !t.text.trim_start().starts_with('['))
        .map(|t| t.p)
        .collect();
    if scored.is_empty() {
        return 0.0;
    }
    scored.iter().sum::<f32>() / scored.len() as f32
}

/// Group ordered [`Word`]s into [`Segment`]s. A segment ends after a word whose text ends in
/// sentence punctuation (`. ! ?`), or before a silence gap wider than [`SEGMENT_GAP_SECS`].
/// Because `-ml 1 -sow` collapses whisper's own segmentation to one-word-per-entry, Cleanroom
/// reconstructs sentence-level segments here rather than paying for a second whisper pass.
fn derive_segments(words: &[Word]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut start_idx = 0usize;

    for i in 0..words.len() {
        let word = &words[i];
        let ends_sentence = word.text.trim_end().ends_with(['.', '!', '?']);
        let gap_after = words
            .get(i + 1)
            .is_some_and(|next| next.start - word.end > SEGMENT_GAP_SECS);
        let is_last = i + 1 == words.len();

        if ends_sentence || gap_after || is_last {
            let group = &words[start_idx..=i];
            segments.push(Segment {
                text: group
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                start: group[0].start,
                end: group[group.len() - 1].end,
                speaker: None,
            });
            start_idx = i + 1;
        }
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative slice captured from a real `whisper-cli -ml 1 -sow -oj -ojf` run
    /// (small.en model), hand-trimmed to two sentences. Exercises: the empty `[_BEG_]` skip,
    /// ms→s offset conversion, a multi-token word with a filtered special token, and segment
    /// splitting on sentence punctuation.
    const SAMPLE_JSON: &str = r#"{
      "systeminfo": "WHISPER : CPU",
      "model": { "type": "small", "multilingual": false },
      "params": { "model": "ggml-small.en.bin", "language": "en", "translate": false },
      "result": { "language": "en" },
      "transcription": [
        {
          "timestamps": { "from": "00:00:00,000", "to": "00:00:00,000" },
          "offsets": { "from": 0, "to": 0 },
          "text": "",
          "tokens": [ { "text": "[_BEG_]", "offsets": { "from": 0, "to": 0 }, "id": 50363, "p": 0.99, "t_dtw": -1 } ]
        },
        {
          "offsets": { "from": 0, "to": 500 },
          "text": " Hello",
          "tokens": [ { "text": " Hello", "p": 0.95 } ]
        },
        {
          "offsets": { "from": 500, "to": 900 },
          "text": " brave",
          "tokens": [ { "text": " brave", "p": 0.80 } ]
        },
        {
          "offsets": { "from": 900, "to": 1100 },
          "text": " new",
          "tokens": [ { "text": " new", "p": 0.99 } ]
        },
        {
          "offsets": { "from": 1100, "to": 1600 },
          "text": " world.",
          "tokens": [
            { "text": " world", "p": 0.90 },
            { "text": ".", "p": 0.50 },
            { "text": "[_TT_50]", "p": 0.99 }
          ]
        },
        {
          "offsets": { "from": 2600, "to": 2900 },
          "text": " Bye",
          "tokens": [ { "text": " Bye", "p": 0.88 } ]
        },
        {
          "offsets": { "from": 2900, "to": 3200 },
          "text": " now.",
          "tokens": [ { "text": " now", "p": 0.92 }, { "text": ".", "p": 0.60 } ]
        }
      ]
    }"#;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "expected ~{b}, got {a}");
    }

    #[test]
    fn parses_words_with_timestamps_and_confidence() {
        let t = parse_whisper_json(SAMPLE_JSON).expect("parse");

        assert_eq!(t.language, "en");
        // Six real words; the empty `[_BEG_]` entry is skipped.
        assert_eq!(t.words.len(), 6);

        let texts: Vec<&str> = t.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Hello", "brave", "new", "world.", "Bye", "now."]);

        // ms -> s conversion.
        approx(t.words[0].start as f32, 0.0);
        approx(t.words[0].end as f32, 0.5);
        approx(t.words[3].start as f32, 1.1);
        approx(t.words[3].end as f32, 1.6);

        // Single-token word: confidence is the token p.
        approx(t.words[0].confidence, 0.95);
        // Multi-token word "world.": mean of (0.90, 0.50); the `[_TT_50]` special is excluded.
        approx(t.words[3].confidence, 0.70);
    }

    #[test]
    fn groups_words_into_sentence_segments() {
        let t = parse_whisper_json(SAMPLE_JSON).expect("parse");

        assert_eq!(t.segments.len(), 2);
        assert_eq!(t.segments[0].text, "Hello brave new world.");
        approx(t.segments[0].start as f32, 0.0);
        approx(t.segments[0].end as f32, 1.6);
        assert_eq!(t.segments[1].text, "Bye now.");
        approx(t.segments[1].start as f32, 2.6);
        approx(t.segments[1].end as f32, 3.2);
    }

    #[test]
    fn empty_transcription_yields_empty_transcript() {
        let json = r#"{ "result": { "language": "de" }, "transcription": [] }"#;
        let t = parse_whisper_json(json).expect("parse");
        assert_eq!(t.language, "de");
        assert!(t.words.is_empty());
        assert!(t.segments.is_empty());
    }

    #[test]
    fn gap_splits_segment_without_punctuation() {
        // No sentence punctuation, but a >0.8 s gap between "two" and "three".
        let json = r#"{
          "result": { "language": "en" },
          "transcription": [
            { "offsets": { "from": 0,    "to": 300 },  "text": " one",   "tokens": [ { "text": " one",   "p": 0.9 } ] },
            { "offsets": { "from": 300,  "to": 600 },  "text": " two",   "tokens": [ { "text": " two",   "p": 0.9 } ] },
            { "offsets": { "from": 1600, "to": 1900 }, "text": " three", "tokens": [ { "text": " three", "p": 0.9 } ] }
          ]
        }"#;
        let t = parse_whisper_json(json).expect("parse");
        assert_eq!(t.words.len(), 3);
        assert_eq!(t.segments.len(), 2);
        assert_eq!(t.segments[0].text, "one two");
        assert_eq!(t.segments[1].text, "three");
    }

    #[test]
    fn language_default_when_missing() {
        let t = Language::default();
        assert_eq!(t.as_arg(), "auto");
        assert_eq!(Language::Code("de".into()).as_arg(), "de");
    }
}
