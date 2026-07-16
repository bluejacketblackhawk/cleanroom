//! # anvil-audio
//!
//! Local audio playback and waveform data for the ANVIL desktop app (M0 lane C).
//!
//! Two responsibilities, both UI-independent (the React layer is a remote control that
//! talks to these through Tauri commands — audio never crosses the webview, ADR-010):
//!
//! - **Playback**: a `cpal` output engine (WASAPI on Windows, CoreAudio on macOS — cpal
//!   picks the host) with transport (play / pause / stop / seek) over an
//!   [`anvil_media::AudioBuffer`]. Device-rate mismatch is reconciled here; the engine's
//!   internal material is always planar f32 @ [`anvil_core::INTERNAL_SAMPLE_RATE`].
//! - **Waveform**: a multi-resolution min/max **peaks pyramid** so the UI can draw a
//!   1-hour file's waveform in well under the M0 budget and re-slice it at any zoom
//!   without touching the raw samples again.
//!
//! Lane C fills in `playback` and `peaks` modules here.

mod engine;
mod peaks;
mod resample;

pub use engine::PlaybackEngine;
pub use peaks::PeaksPyramid;
pub use resample::resample_planar;

/// Errors from the playback engine. Waveform building is infallible (empty in → empty out).
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The host reported no default output device.
    #[error("no default audio output device")]
    NoOutputDevice,
    /// The output device could not be opened or configured.
    #[error("audio device error: {0}")]
    Device(String),
    /// The audio worker thread has exited (e.g. after shutdown).
    #[error("audio thread is no longer running")]
    AudioThreadGone,
}
