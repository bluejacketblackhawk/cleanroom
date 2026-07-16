//! # anvil-multitrack
//!
//! Multitrack production (03 §6): the double-ender / remote-interview path.
//!
//! - **Alignment** ([`align`]): GCC-PHAT cross-correlation to find the constant offset between
//!   tracks recorded on different machines, plus a drift line (≤ 50 ppm resample repair) for
//!   cheap recorders whose clocks disagree. Confidence is surfaced per track.
//! - **Crossgate (bleed control)** ([`crossgate`]): when speaker A is dominant and track B
//!   carries a delayed/attenuated copy of A (spill), duck B — but never gate B's own speech
//!   onsets. This is Auphonic's multitrack magic (P11); getting it right is the point of the
//!   lane. The discriminator is *how much of B's frame A explains* — see the [`crossgate`]
//!   module docs, which is where the interesting part of this crate lives.
//! - **Ducking** ([`duck`]): music/SFX tracks duck under speech with a 200 ms lookahead
//!   fade-down, an 800 ms fade-up, and a 300 ms hold so the bed never chatters between words.
//! - **Per-track chains + mixdown** ([`mix`]): each speech track runs the §4 chain (the
//!   leveler doing per-speaker balance); music tracks get a light chain; the sum goes to the
//!   master bus (§4.9 two-pass loudness + §4.10 true-peak limiter).
//!
//! ```no_run
//! use anvil_multitrack::{mix, MultitrackOptions, Track};
//!
//! let tracks = [
//!     Track::speech("host.wav", "Host"),
//!     Track::speech("guest.wav", "Guest"),
//!     Track::music("bed.wav", "Intro bed"),
//! ];
//! let result = mix(&tracks, &MultitrackOptions::default())?;
//! println!("{:+.3} s apart", result.alignment.offsets_secs[1]);
//! # Ok::<(), anvil_multitrack::MultitrackError>(())
//! ```
//!
//! Everything here is **deterministic** (ADR-003): no time, no threads, no entropy. Identical
//! inputs and options produce a bit-identical mix.
//!
//! ## Known limits (the honest ones)
//!
//! - The crossgate models the bleed path as a **single tap** (one delay, one gain). A real
//!   room adds a reverberant tail that one tap cannot explain, so that tail lands in the
//!   residual and pushes the veto toward "B is talking". The bias is deliberate — leaving
//!   bleed in is a blemish, chopping a syllable is a bug — but a multi-tap FIR fit of the
//!   A→B path would duck more of the spill in a live room. See [`CrossgateConfig`].
//! - With three or more speech tracks, a frame's bleed is attributed to the **single
//!   best-explaining** source rather than to a joint projection over all of them. Exact for a
//!   double-ender; an approximation for a panel where two people bleed into one mic at once.

pub mod align;
pub mod crossgate;
pub mod duck;
pub mod error;
pub mod mix;
pub mod track;
mod vad;

#[cfg(test)]
mod testfix;

pub use align::{align_buffers, apply_offset, repair_drift, AlignConfig, Alignment};
pub use crossgate::{crossgate, CrossgateConfig, CrossgateReport, CrossgateResult};
pub use duck::{duck_gain, DuckConfig, DuckReport};
pub use error::MultitrackError;
pub use mix::{mix, mix_buffers, MixReport, MixResult, TrackReport};
pub use track::{MultitrackOptions, Track, TrackKind};

use anvil_media::decode_to_buffer;

/// Align a set of tracks against the first one (03 §6).
///
/// Decodes each track, then returns the constant offsets, the drift line (ppm), and the
/// confidence. [`align_buffers`] is the same thing for audio already in memory.
pub fn align(tracks: &[Track]) -> Result<Alignment, MultitrackError> {
    if tracks.is_empty() {
        return Err(MultitrackError::NoTracks);
    }
    let mut buffers = Vec::with_capacity(tracks.len());
    for t in tracks {
        let buf = decode_to_buffer(&t.path).map_err(|source| MultitrackError::Decode {
            name: t.name.clone(),
            source,
        })?;
        if buf.is_empty() {
            return Err(MultitrackError::EmptyTrack(t.name.clone()));
        }
        buffers.push(buf);
    }
    Ok(align_buffers(&buffers, &AlignConfig::default()))
}
