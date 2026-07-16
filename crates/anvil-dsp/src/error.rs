//! Error type for the DSP crate.

use thiserror::Error;

/// Errors from analysis and mastering.
#[derive(Error, Debug)]
pub enum DspError {
    /// Decoding / media IO failed (propagated from `anvil-media`).
    #[error("media: {0}")]
    Media(#[from] anvil_media::MediaError),

    /// The BS.1770-4 loudness meter (`ebur128`) rejected our input or state.
    #[error("loudness meter: {0}")]
    Meter(String),

    /// The input had no audio frames (0-byte / empty file). Callers get a friendly no-op
    /// rather than a panic (03 §8 edge cases).
    #[error("no audio frames in input")]
    Empty,

    /// The output sink (incremental WAV writer / ffmpeg encoder) failed while the streaming
    /// master was feeding it processed blocks.
    #[error("output: {0}")]
    Sink(String),
}

impl From<ebur128::Error> for DspError {
    fn from(e: ebur128::Error) -> Self {
        DspError::Meter(format!("{e:?}"))
    }
}
