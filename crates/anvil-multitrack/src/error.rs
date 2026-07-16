//! Error type for the multitrack lane.

use thiserror::Error;

/// Errors from alignment, crossgating, ducking, and mixdown.
#[derive(Error, Debug)]
pub enum MultitrackError {
    /// `mix` was handed an empty track list.
    #[error("no tracks to mix")]
    NoTracks,

    /// A track decoded to zero frames (0-byte / silent-length file). 03 §8: graceful, not a panic.
    #[error("track \"{0}\" has no audio frames")]
    EmptyTrack(String),

    /// Decoding a track failed (propagated from `anvil-media`, tagged with which track).
    #[error("track \"{name}\": {source}")]
    Decode {
        /// The track that failed.
        name: String,
        /// The underlying media error.
        #[source]
        source: anvil_media::MediaError,
    },

    /// Every track is muted, or a solo is active but every soloed track is also muted.
    #[error("nothing audible: every track is muted (or the soloed tracks are)")]
    NothingAudible,

    /// [`crate::mix_buffers`] was handed a different number of buffers than tracks.
    #[error("track/buffer count mismatch: {tracks} tracks, {buffers} buffers")]
    BufferCountMismatch {
        /// Number of [`crate::Track`]s.
        tracks: usize,
        /// Number of buffers.
        buffers: usize,
    },

    /// The DSP crate failed (loudness meter, chain).
    #[error("dsp: {0}")]
    Dsp(#[from] anvil_dsp::DspError),
}
