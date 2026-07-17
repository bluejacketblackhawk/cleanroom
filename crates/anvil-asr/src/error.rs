//! Error type for the speech-recognition lane.
//!
//! Kept local to `anvil-asr` (the workspace rule is that engine crates own their error
//! surface). A `From<AsrError> for anvil_core::Error` is provided so callers that work in
//! [`anvil_core::Result`] — the CLI, the job system, the cut engine — can bubble ASR
//! failures up with `?` without this crate having to edit `anvil-core`.

/// Failures that can arise while locating whisper, resolving a model, transcribing, or
/// parsing whisper.cpp's JSON output.
#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    /// Filesystem / pipe IO failure (spawning whisper, reading its JSON file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// whisper.cpp's `-oj`/`-ojf` JSON did not deserialize into the expected shape.
    #[error("whisper JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// The `whisper-cli` binary could not be located (airplane-mode: we never auto-download
    /// it — see [`crate::WhisperSidecar::locate`]).
    #[error("whisper sidecar not found: {0}")]
    SidecarNotFound(String),

    /// `whisper-cli` ran but exited non-zero; payload is the captured tail of stderr.
    #[error("whisper sidecar failed: {0}")]
    SidecarFailed(String),

    /// No ggml model could be resolved (none given, `CLEANROOM_WHISPER_MODEL` unset, and none
    /// installed in the models dir). Airplane-mode: locating a model never downloads one.
    #[error("whisper model not found: {0}")]
    ModelNotFound(String),

    /// A model file is present but does not match its catalogued size/SHA-256 — a truncated
    /// or tampered download. See [`crate::verify_model`].
    #[error("model failed hash verification: {0}")]
    ModelCorrupt(String),

    /// The diarization sidecar ran but its output could not be understood (a sherpa-onnx
    /// version whose stdout format we do not recognise, or a run that emitted nothing).
    #[error("diarization output parse error: {0}")]
    DiarizeParse(String),

    /// The audio could not be prepared for the diarization sidecar, which — unlike whisper —
    /// resamples nothing and requires 16 kHz mono 16-bit PCM WAV. See
    /// [`crate::diarize`](crate::diarize) for the ffmpeg fallback.
    #[error("audio not usable for diarization: {0}")]
    AudioUnsupported(String),
}

/// Fold an ASR error into the workspace-wide [`anvil_core::Error`]. Defined here (not in
/// anvil-core) so this crate stays self-contained and anvil-core is never edited.
impl From<AsrError> for anvil_core::Error {
    fn from(err: AsrError) -> Self {
        match err {
            AsrError::Io(e) => anvil_core::Error::Io(e),
            AsrError::Json(e) => anvil_core::Error::Json(e),
            other => anvil_core::Error::Other(other.to_string()),
        }
    }
}
