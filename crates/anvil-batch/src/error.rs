//! Error type for the batch/watch lane.
//!
//! Kept local to `anvil-batch` (the workspace rule is that engine crates own their error
//! surface — see `anvil_media::MediaError` / `anvil_dsp::DspError`). A
//! `From<BatchError> for anvil_core::Error` conversion is provided so job closures
//! submitted to [`anvil_core::job::JobScheduler`] (whose `submit` requires
//! `anvil_core::Result<T>`) can bubble batch/watch failures up with `?`.

use std::path::PathBuf;

use thiserror::Error;

/// Failures that can arise while queuing, rendering, or watching for files.
#[derive(Debug, Error)]
pub enum BatchError {
    /// Filesystem IO failure (reading a folder to scan, creating an output directory,
    /// writing the interim WAV, reading/writing the watch processed-log, ...).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The mastering pass itself failed (propagated from `anvil-dsp`).
    #[error("mastering failed: {0}")]
    Dsp(#[from] anvil_dsp::DspError),

    /// Writing the interim WAV output failed (the encoder seam — see `queue::render_job`).
    #[error("WAV write error: {0}")]
    Wav(#[from] hound::Error),

    /// The `notify` filesystem watcher failed to start or configure a watch.
    #[error("watcher error: {0}")]
    Notify(#[from] notify::Error),

    /// A watch rule's folder can't be read (moved, unmounted, permissions) — surfaces as
    /// the rule's "unreachable" badge (04 §S5 error states) rather than crashing the
    /// service.
    #[error("watch folder unreachable: {0}")]
    FolderUnreachable(PathBuf),

    /// Anything else, with a human-readable message.
    #[error("{0}")]
    Other(String),
}

impl From<BatchError> for anvil_core::Error {
    fn from(err: BatchError) -> Self {
        match err {
            BatchError::Io(e) => anvil_core::Error::Io(e),
            other => anvil_core::Error::Other(other.to_string()),
        }
    }
}
