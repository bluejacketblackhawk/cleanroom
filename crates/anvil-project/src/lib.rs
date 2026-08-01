//! # anvil-project
//!
//! The project, preset, and settings model (02 §Project & data model). An `.anvilproj`
//! is a schema-versioned folder (sources + analysis cache + EDL + render history);
//! presets are shareable JSON documents; settings live in the platform config dir.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use anvil_core::{platform::Platform, Result};

pub mod autosave;
pub mod compliance;
pub mod edl;
pub mod preset;
pub mod project;
pub mod voice_memory;

pub use autosave::Autosave;
pub use compliance::{AcxCheck, AcxConform, ComplianceInput, LoudnessMeasurement, ModuleDecision};
pub use edl::{Edl, EdlSource, Seconds, Segment};
pub use preset::{AnvilPreset, ShippedPreset, PRESET_FILE_EXTENSION};
pub use project::{AnalysisCache, Project, ProjectManifest, RenderHistory, PROJECT_MANIFEST_FILE};
pub use voice_memory::{EqBand, SpeakerProfile, VoiceMemory};

/// Bumped on any breaking change to the on-disk project/preset format; migrations key
/// off it.
pub const PROJECT_SCHEMA_VERSION: u32 = 1;

/// Processing quality tier (03 §quality tiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Lightweight denoise, lowest latency (RNNoise/GTCRN).
    Fast,
    /// Default: DeepFilterNet3 + full leveling chain.
    Standard,
    /// GPU-preferred enhancement (dereverb + BWE + separation), M4.
    Studio,
}

/// A named processing preset: the chain parameters, loudness target, and outputs.
/// Serialized as a shareable JSON document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub schema_version: u32,
    pub name: String,
    pub tier: Tier,
    /// Integrated loudness target in LUFS (e.g. -16 for stereo podcast).
    pub target_lufs: f32,
    /// True-peak ceiling in dBTP; the limiter never exceeds it (06: zero tolerance).
    pub true_peak_ceiling_dbtp: f32,
}

impl Default for Preset {
    /// The shipped `podcast-stereo` default: Standard tier, −16 LUFS, −1 dBTP ceiling.
    fn default() -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            name: "podcast-stereo".into(),
            tier: Tier::Standard,
            target_lufs: -16.0,
            true_peak_ceiling_dbtp: -1.0,
        }
    }
}

/// Application settings persisted to the platform config dir.
///
/// Every field added after 1.0 carries `#[serde(default)]` so a settings.json written by an
/// older build keeps loading (the additive contract the desktop Settings screen relies on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub schema_version: u32,
    pub default_preset: String,
    /// Default cut word/phrase for the Split flow (09 §1 "picking the cutword … in
    /// settings") — `None` until the user picks one.
    #[serde(default)]
    pub default_cut_word: Option<String>,
    /// Split output naming template (09 §5). Tokens: `{n}`, `{nn}`, `{name}`,
    /// `{first_words}`.
    #[serde(default = "default_split_name_template")]
    pub split_name_template: String,
    /// Whether Split masters each segment (09 §4) — on unless the user opts out.
    #[serde(default = "default_split_master")]
    pub split_master_default: bool,
}

fn default_split_name_template() -> String {
    "{nn} {first_words}".into()
}

fn default_split_master() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            default_preset: "podcast-stereo".into(),
            default_cut_word: None,
            split_name_template: default_split_name_template(),
            split_master_default: default_split_master(),
        }
    }
}

impl Settings {
    /// The settings file path in the platform config dir (`settings.json`), per
    /// ADR-008. Real load/save call sites use this; tests pass their own tempdir path
    /// instead so they never touch the user's real config dir.
    pub fn default_path() -> PathBuf {
        anvil_core::platform::current()
            .config_dir()
            .join("settings.json")
    }

    /// Load settings from `path`. First run (file doesn't exist yet) yields
    /// [`Settings::default`] rather than an error.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist settings to `path`, creating parent directories as needed.
    /// Write-temp-then-rename for crash safety, matching [`crate::Project::save`].
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_roundtrips_json() {
        let p = Preset::default();
        let json = serde_json::to_string_pretty(&p).unwrap();
        let back: Preset = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn default_preset_targets_minus_16() {
        let p = Preset::default();
        assert_eq!(p.target_lufs, -16.0);
        assert_eq!(p.tier, Tier::Standard);
    }

    #[test]
    fn settings_load_missing_file_yields_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
    }

    #[test]
    fn settings_save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("settings.json");

        let settings = Settings {
            schema_version: PROJECT_SCHEMA_VERSION,
            default_preset: "custom-preset".into(),
            default_cut_word: Some("kumquat".into()),
            split_name_template: "{n}-{first_words}".into(),
            split_master_default: false,
        };
        settings.save(&path).unwrap();

        assert_eq!(Settings::load(&path).unwrap(), settings);
    }

    /// The additive-settings contract: a settings.json written before the split fields
    /// existed still loads, landing on the documented defaults.
    #[test]
    fn settings_without_split_fields_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{ "schema_version": 1, "default_preset": "podcast-stereo" }"#,
        )
        .unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.default_cut_word, None);
        assert_eq!(s.split_name_template, "{nn} {first_words}");
        assert!(s.split_master_default);
    }

    #[test]
    fn settings_save_leaves_no_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        Settings::default().save(&path).unwrap();
        assert!(!tmp.path().join("settings.json.tmp").exists());
    }
}
