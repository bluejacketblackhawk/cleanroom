//! Core error type shared across the engine.

/// Errors surfaced by the Cleanroom core and the crates layered on it.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported media format: {0}")]
    UnsupportedFormat(String),

    #[error("job was cancelled")]
    Cancelled,

    #[error("job panicked: {0}")]
    JobPanicked(String),

    #[error("unsupported project schema version {found} (this build supports up to {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },

    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    #[error("{0}")]
    Other(String),
}

/// Convenience alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, Error>;
