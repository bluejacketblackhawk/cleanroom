//! End-to-end transcription against a real `whisper-cli` + ggml model.
//!
//! This test is **gated on the environment** so `cargo test` stays green (and offline) on
//! machines without whisper installed. It runs only when all three are set and point at real
//! files:
//! - `CLEANROOM_WHISPER`        — path to `whisper-cli`(`.exe`),
//! - `CLEANROOM_WHISPER_MODEL`  — path to a `ggml-*.bin`,
//! - `CLEANROOM_ASR_TEST_AUDIO` — path to a short audio file (ideally 16 kHz mono WAV).
//!
//! When any is missing the test prints a skip note and passes.

use std::path::PathBuf;

use anvil_asr::{transcribe, Language, TranscribeOptions, WhisperSidecar};

fn env_file(key: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(key)?);
    path.is_file().then_some(path)
}

#[test]
fn real_transcribe_when_whisper_available() {
    let (Some(whisper), Some(model), Some(audio)) = (
        env_file("CLEANROOM_WHISPER"),
        env_file("CLEANROOM_WHISPER_MODEL"),
        env_file("CLEANROOM_ASR_TEST_AUDIO"),
    ) else {
        eprintln!(
            "skipping real transcribe: set CLEANROOM_WHISPER, CLEANROOM_WHISPER_MODEL, and \
             CLEANROOM_ASR_TEST_AUDIO (all pointing at real files) to run it"
        );
        return;
    };

    // Sanity-check the sidecar locator resolves the same binary from CLEANROOM_WHISPER.
    let sidecar = WhisperSidecar::locate().expect("locate whisper via CLEANROOM_WHISPER");
    assert_eq!(sidecar.binary(), whisper.as_path());

    let opts = TranscribeOptions {
        language: Language::Auto,
        model: Some(model),
        threads: None,
    };
    let transcript = transcribe(&audio, &opts).expect("transcribe should succeed");

    assert!(
        !transcript.words.is_empty(),
        "expected at least one recognized word"
    );
    assert!(
        !transcript.segments.is_empty(),
        "expected at least one segment"
    );

    // Timestamps must be ordered and non-negative, and confidences in range.
    for word in &transcript.words {
        assert!(word.start >= 0.0 && word.end >= word.start, "{word:?}");
        assert!(
            (0.0..=1.0).contains(&word.confidence),
            "confidence out of range: {word:?}"
        );
    }
    for pair in transcript.words.windows(2) {
        assert!(pair[1].start >= pair[0].start, "words must be time-ordered");
    }

    eprintln!(
        "transcribed {} words / {} segments (lang={}): {:?}",
        transcript.words.len(),
        transcript.segments.len(),
        transcript.language,
        transcript.text()
    );
}
