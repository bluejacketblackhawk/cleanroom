//! # anvil-core
//!
//! The UI-independent heart of Cleanroom: error types, the cancellable job system, the
//! processing-graph model (added in M0.E / M1), analysis orchestration, and the
//! [`platform`] abstraction that is the *only* place OS-specific `#[cfg]` branching is
//! allowed (ADR-006).
//!
//! Crates in this workspace never depend on Tauri; `apps/desktop` depends on these.

pub mod error;
pub mod job;
pub mod platform;

pub use error::{Error, Result};

/// Processing-chain version. Bumped whenever the DSP/AI graph can produce different
/// output; projects record it so re-renders stay bit-identical forever (ADR-003,
/// feature #11 "sound-version pinning").
///
/// v5 (2026-07): the loudness normalize now drives make-up gain into the true-peak limiter
/// and *converges* the limited integrated loudness onto target (bounded iteration over the
/// spill/buffer), replacing the single flat-trim correction that left high-crest content up to
/// several LU below target. Any file whose old master engaged the limiter renders differently.
pub const CHAIN_VERSION: u32 = 5;

/// Internal processing sample rate in Hz — DeepFilterNet3's native rate (ADR-002).
pub const INTERNAL_SAMPLE_RATE: u32 = 48_000;

/// Fixed processing hop in samples: 10 ms @ 48 kHz (ADR-002).
pub const HOP_SAMPLES: usize = 480;

/// Block size in samples: 100 ms @ 48 kHz (`HOP_SAMPLES * 10`) (ADR-002).
pub const BLOCK_SAMPLES: usize = 4_800;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_ten_hops() {
        assert_eq!(BLOCK_SAMPLES, HOP_SAMPLES * 10);
        assert_eq!(HOP_SAMPLES as u32 * 100, INTERNAL_SAMPLE_RATE);
    }
}
