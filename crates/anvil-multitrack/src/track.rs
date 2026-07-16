//! The multitrack contract: [`Track`], [`TrackKind`], and [`MultitrackOptions`] (03 §6).
//!
//! Everything here is `serde` + `snake_case` — it is the wire format the CLI, the desktop
//! app, and the project file all speak.

use std::path::{Path, PathBuf};

use anvil_project::Tier;
use serde::{Deserialize, Serialize};

use crate::align::AlignConfig;
use crate::crossgate::CrossgateConfig;
use crate::duck::DuckConfig;

/// What a track *is*, which decides the chain it gets (03 §6: "N speech tracks + M music/SFX
/// tracks (user-tagged, auto-guessed from analysis)").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    /// A microphone track: full §4 chain, crossgated against the other speech tracks.
    Speech,
    /// Music / SFX bed: light chain (HPF + loudness prep), ducked under speech.
    Music,
}

/// One input track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Source file.
    pub path: PathBuf,
    /// Speech mic or music/SFX bed.
    pub kind: TrackKind,
    /// Display name (also the key in the mix report).
    pub name: String,
    /// User gain offset in dB, applied after the per-track chain.
    #[serde(default)]
    pub gain_db: f32,
    /// Solo: when any track is soloed, only soloed tracks are audible.
    #[serde(default)]
    pub solo: bool,
    /// Mute: excluded from the mix. A muted track still *contributes* to the crossgate and
    /// duck decisions — its bleed is still in everyone else's mic.
    #[serde(default)]
    pub mute: bool,
}

impl Track {
    /// A speech track with unity gain.
    pub fn speech(path: impl AsRef<Path>, name: impl Into<String>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            kind: TrackKind::Speech,
            name: name.into(),
            gain_db: 0.0,
            solo: false,
            mute: false,
        }
    }

    /// A music/SFX track with unity gain.
    pub fn music(path: impl AsRef<Path>, name: impl Into<String>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            kind: TrackKind::Music,
            name: name.into(),
            gain_db: 0.0,
            solo: false,
            mute: false,
        }
    }

    /// Is this a speech track?
    pub fn is_speech(&self) -> bool {
        self.kind == TrackKind::Speech
    }
}

/// Ducking depth bounds (03 §6: "default −12, range −6..−24").
pub const DUCK_DB_MIN: f32 = -24.0;
/// Ducking depth bounds (03 §6).
pub const DUCK_DB_MAX: f32 = -6.0;
/// Crossgate depth bound (03 §6: "duck B by up to −15 dB").
pub const CROSSGATE_DB_MIN: f32 = -24.0;

/// Multitrack options. Defaults are the spec's defaults; the UI exposes `duck_db`,
/// `crossgate_db`, solo/mute, and the per-track gains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MultitrackOptions {
    /// How far music ducks under speech, dB (03 §6: default −12, range −6..−24).
    pub duck_db: f32,
    /// How far a speech track ducks when it is carrying another speaker's bleed, dB
    /// (03 §6: "up to −15 dB").
    pub crossgate_db: f32,
    /// Run the crossgate at all.
    pub crossgate: bool,
    /// Run alignment (GCC-PHAT offsets). Off = tracks are assumed sample-aligned already.
    pub align: bool,
    /// Repair clock drift by resampling (≤ `align_config.max_drift_ppm`).
    pub drift_repair: bool,
    /// Run the per-track §4 chain. Off = tracks are summed as decoded (fast preview path).
    pub per_track_chain: bool,
    /// Run the AI denoiser inside the per-track chain. Off keeps the rest of the chain.
    pub denoise: bool,
    /// Master-bus integrated loudness target, LUFS (§4.9).
    pub target_lufs: f32,
    /// Master-bus true-peak ceiling, dBTP (§4.10).
    pub true_peak_ceiling_dbtp: f32,
    /// Quality tier for the per-track chains.
    pub tier: Tier,
    /// Mixdown channel count (1 = mono, 2 = stereo).
    pub output_channels: usize,
    /// Alignment tuning.
    pub align_config: AlignConfig,
    /// Crossgate tuning.
    pub crossgate_config: CrossgateConfig,
    /// Ducking tuning.
    pub duck_config: DuckConfig,
}

impl Default for MultitrackOptions {
    fn default() -> Self {
        Self {
            duck_db: -12.0,
            crossgate_db: -15.0,
            crossgate: true,
            align: true,
            drift_repair: true,
            per_track_chain: true,
            denoise: true,
            target_lufs: -16.0,
            true_peak_ceiling_dbtp: -1.0,
            tier: Tier::Standard,
            output_channels: 2,
            align_config: AlignConfig::default(),
            crossgate_config: CrossgateConfig::default(),
            duck_config: DuckConfig::default(),
        }
    }
}

impl MultitrackOptions {
    /// The ducking depth, clamped to the spec's range (−6 .. −24 dB).
    pub fn duck_db_clamped(&self) -> f32 {
        self.duck_db.clamp(DUCK_DB_MIN, DUCK_DB_MAX)
    }

    /// The crossgate depth, clamped to −24 .. 0 dB.
    pub fn crossgate_db_clamped(&self) -> f32 {
        self.crossgate_db.clamp(CROSSGATE_DB_MIN, 0.0)
    }

    /// The resolved duck config (options override the tuning struct's depth).
    pub fn resolved_duck(&self) -> DuckConfig {
        DuckConfig {
            duck_db: self.duck_db_clamped(),
            ..self.duck_config
        }
    }

    /// The resolved crossgate config (options override the tuning struct's depth).
    pub fn resolved_crossgate(&self) -> CrossgateConfig {
        CrossgateConfig {
            max_reduction_db: self.crossgate_db_clamped(),
            ..self.crossgate_config
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_keys_are_snake_case() {
        let t = Track::speech("a.wav", "Host");
        let v = serde_json::to_value(&t).unwrap();
        for key in ["path", "kind", "name", "gain_db", "solo", "mute"] {
            assert!(v.get(key).is_some(), "Track missing key {key}");
        }
        assert_eq!(v["kind"], "speech");
        assert_eq!(serde_json::to_value(TrackKind::Music).unwrap(), "music");

        let o = serde_json::to_value(MultitrackOptions::default()).unwrap();
        for key in [
            "duck_db",
            "crossgate_db",
            "target_lufs",
            "true_peak_ceiling_dbtp",
            "tier",
            "output_channels",
        ] {
            assert!(o.get(key).is_some(), "MultitrackOptions missing key {key}");
        }
        assert_eq!(o["duck_db"], -12.0);
        assert_eq!(o["crossgate_db"], -15.0);
    }

    #[test]
    fn duck_depth_is_clamped_to_the_spec_range() {
        let mut o = MultitrackOptions {
            duck_db: -40.0,
            ..Default::default()
        };
        assert_eq!(o.duck_db_clamped(), DUCK_DB_MIN);
        o.duck_db = 0.0;
        assert_eq!(o.duck_db_clamped(), DUCK_DB_MAX);
        o.duck_db = -12.0;
        assert_eq!(o.duck_db_clamped(), -12.0);
    }

    #[test]
    fn options_deserialize_from_a_partial_document() {
        let o: MultitrackOptions = serde_json::from_str(r#"{"duck_db": -18.0}"#).unwrap();
        assert_eq!(o.duck_db, -18.0);
        assert_eq!(o.crossgate_db, -15.0, "unset fields keep their defaults");
    }
}
