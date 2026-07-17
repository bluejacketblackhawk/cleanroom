//! Error type for the media-IO lane.
//!
//! Kept local to `anvil-media` (the workspace rule is that engine crates own their error
//! surface). A `From<MediaError> for anvil_core::Error` is provided so callers that work
//! in [`anvil_core::Result`] — the CLI, the job system — can bubble media failures up with
//! `?` without this crate having to edit `anvil-core`.

/// Failures that can arise while probing, decoding, or shelling out to the ffmpeg sidecar.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// Filesystem / pipe IO failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The container or codec is not handled by symphonia *and* no ffmpeg sidecar was
    /// available to fall back to. This is the variant the fallback chain keys off of.
    #[error("unsupported media format: {0}")]
    UnsupportedFormat(String),

    /// The file was recognized but carries no decodable audio track.
    #[error("no audio track in {0}")]
    NoAudioTrack(String),

    /// symphonia demux/decode error that is not simply "unsupported".
    #[error("decode error: {0}")]
    Decode(String),

    /// rubato construction or processing error during 48 kHz resampling.
    #[error("resample error: {0}")]
    Resample(String),

    /// The pinned ffmpeg sidecar binary could not be located (airplane-mode: we never
    /// auto-download it).
    #[error("ffmpeg sidecar not found: {0}")]
    SidecarNotFound(String),

    /// The ffmpeg sidecar ran but exited non-zero; payload is the captured tail of stderr.
    #[error("ffmpeg sidecar failed: {0}")]
    SidecarFailed(String),

    /// The located ffmpeg binary did not match the pinned sha256 — refuse to run it.
    ///
    /// Also carries the other "we refuse to run this sidecar for a licence/supply-chain reason"
    /// cases so the enum's public shape (matched exhaustively by the CLI) stays unchanged: an
    /// unpinned binary we won't run, a platform with no vendored pin yet, and a GPL build caught
    /// by [`crate::sidecar::FfmpegSidecar::assert_lgpl`] all surface here or as
    /// [`Self::SidecarFailed`]. `assert_lgpl` also returns the offending configure markers, and
    /// [`crate::sidecar::gpl_markers_in`] exposes them for structured checks.
    #[error(
        "ffmpeg sidecar hash mismatch: expected {expected}, got {actual} \
         (run scripts/fetch-ffmpeg.ps1 to provision the pinned LGPL build; a developer using \
         their own ffmpeg must set CLEANROOM_FFMPEG and CLEANROOM_FFMPEG_ALLOW_UNPINNED=1)"
    )]
    SidecarHashMismatch { expected: String, actual: String },

    /// lofty tag read/write failure (M2: metadata/chapters lane).
    #[error("metadata error: {0}")]
    Metadata(String),

    /// A [`crate::clip::ClipSpec`] could not be rendered as asked: an empty/inverted time range,
    /// a colour that isn't `#RRGGBB`, missing cover art, or a video encoder that fails the
    /// licence bar (handoff/07 §2 — never GPL-linked).
    #[error("invalid clip: {0}")]
    InvalidClip(String),
}

impl From<lofty::error::LoftyError> for MediaError {
    fn from(err: lofty::error::LoftyError) -> Self {
        MediaError::Metadata(err.to_string())
    }
}

impl From<symphonia::core::errors::Error> for MediaError {
    fn from(err: symphonia::core::errors::Error) -> Self {
        use symphonia::core::errors::Error as S;
        match err {
            S::IoError(e) => MediaError::Io(e),
            // "Unsupported" is symphonia's signal that it can't handle this container/codec;
            // map it to the fallback-triggering variant so we try the ffmpeg sidecar.
            S::Unsupported(what) => MediaError::UnsupportedFormat(what.to_string()),
            other => MediaError::Decode(other.to_string()),
        }
    }
}

impl From<rubato::ResamplerConstructionError> for MediaError {
    fn from(err: rubato::ResamplerConstructionError) -> Self {
        MediaError::Resample(err.to_string())
    }
}

impl From<rubato::ResampleError> for MediaError {
    fn from(err: rubato::ResampleError) -> Self {
        MediaError::Resample(err.to_string())
    }
}

/// Fold a media error into the workspace-wide [`anvil_core::Error`]. Defined here (not in
/// anvil-core) so this crate stays self-contained and anvil-core is never edited.
impl From<MediaError> for anvil_core::Error {
    fn from(err: MediaError) -> Self {
        match err {
            MediaError::Io(e) => anvil_core::Error::Io(e),
            MediaError::UnsupportedFormat(s) => anvil_core::Error::UnsupportedFormat(s),
            other => anvil_core::Error::Other(other.to_string()),
        }
    }
}
