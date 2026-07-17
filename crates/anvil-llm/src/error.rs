//! Error type for the local-LLM lane.
//!
//! Kept local to `anvil-llm` (the workspace rule is that engine crates own their error
//! surface). A `From<LlmError> for anvil_core::Error` is provided so callers that work in
//! [`anvil_core::Result`] — the CLI, the job system, the metadata tab — can bubble LLM
//! failures up with `?` without this crate having to edit `anvil-core`.
//!
//! Note that most callers should never see these: [`crate::suggest`] degrades to the
//! no-LLM [`crate::fallback`] instead of failing, so a missing model pack is a quality
//! downgrade, not an error.

/// Failures that can arise while locating llama.cpp, resolving a model pack, generating, or
/// parsing the model's JSON output.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Filesystem / pipe IO failure (writing the prompt file, spawning llama-cli).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The model's output did not deserialize into the expected shape.
    #[error("LLM JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// The `llama-cli` binary could not be located (airplane-mode: we never auto-download
    /// it — see [`crate::LlamaSidecar::locate`]).
    #[error("llama sidecar not found: {0}")]
    SidecarNotFound(String),

    /// `llama-cli` ran but exited non-zero; payload is the captured tail of stderr.
    #[error("llama sidecar failed: {0}")]
    SidecarFailed(String),

    /// No gguf model pack could be resolved (none given, `CLEANROOM_LLM_MODEL` unset, and none
    /// installed in the models dir). Airplane-mode: locating a model never downloads one.
    #[error("LLM model pack not found: {0}")]
    ModelNotFound(String),

    /// A model pack's sha256 does not match the pinned value in the catalog — the file is
    /// corrupt or was swapped. Never load it.
    #[error("model pack hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    /// A model pack was used in a way its catalog shape does not allow (e.g. verifying a
    /// multi-shard pack via the single-file entry point). A caller bug, not a bad download.
    #[error("model pack shape error: {0}")]
    ModelCorrupt(String),

    /// The model produced no parseable JSON object at all (empty output, prose-only answer,
    /// truncated generation). Callers typically fall back to [`crate::fallback`].
    #[error("no JSON object in model output: {0}")]
    NoJson(String),
}

/// Fold an LLM error into the workspace-wide [`anvil_core::Error`]. Defined here (not in
/// anvil-core) so this crate stays self-contained and anvil-core is never edited.
impl From<LlmError> for anvil_core::Error {
    fn from(err: LlmError) -> Self {
        match err {
            LlmError::Io(e) => anvil_core::Error::Io(e),
            LlmError::Json(e) => anvil_core::Error::Json(e),
            other => anvil_core::Error::Other(other.to_string()),
        }
    }
}
