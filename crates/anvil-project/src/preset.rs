//! Shipped preset registry and the shareable `.anvilpreset` file format (04 §S6 "Presets
//! manager"): presets ship as data, not code. [`Preset::shipped`]/[`Preset::by_id`] are the
//! single source of truth for the seven built-in targets (03 §4.9) — this replaces the two
//! separate copies the CLI and desktop each grew before this module existed (both were doing
//! their own `match` on preset names/ids; see M2 handoff notes).
//!
//! [`AnvilPreset`] is the on-disk `.anvilpreset` document: a [`Preset`] plus any chain-
//! parameter deltas from the default chain, shareable between shows or committed to a repo.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use anvil_core::{Error, Result};

use crate::{Preset, Tier, PROJECT_SCHEMA_VERSION};

// ---- Stable shipped-preset ids -------------------------------------------------------------
//
// These strings are a contract: the desktop UI, CLI, and any `.anvilpreset`/`ComplianceInput`
// that references a shipped preset by id all key off these exact values. Never rename one
// once shipped — add a new id and leave the old preset resolvable if it must be retired.

/// `Podcast (Stereo −16)` — the default preset (03 §4.9, matches [`Preset::default`]).
pub const PODCAST_STEREO_ID: &str = "podcast_stereo";
/// `Podcast (Mono −19)`.
pub const PODCAST_MONO_ID: &str = "podcast_mono";
/// `Spotify/YouTube −14`.
pub const SPOTIFY_YOUTUBE_ID: &str = "spotify_youtube";
/// `Broadcast EBU −23` (EBU R128).
pub const BROADCAST_EBU_ID: &str = "broadcast_ebu";
/// `Audiobook (ACX)` — special-cased in the compliance report (03 §4.9: RMS window + peak +
/// noise-floor checks, not a plain LUFS target).
pub const AUDIOBOOK_ACX_ID: &str = "audiobook_acx";
/// `Voice memo cleanup` — single-voice, low-stakes, lowest-latency tier.
pub const VOICE_MEMO_CLEANUP_ID: &str = "voice_memo_cleanup";
/// `Music-heavy show` — lighter denoise so music beds survive (03 §2: "Music-majority file
/// ... denoise defaults lighter").
pub const MUSIC_HEAVY_SHOW_ID: &str = "music_heavy_show";

/// ACX's true-peak ceiling (03 §4.10): tighter than the −1.0 dBTP default because ACX's own
/// submission checklist caps peak at −3 dBFS.
pub const ACX_TRUE_PEAK_CEILING_DBTP: f32 = -3.0;

/// One shipped preset: its stable id (see the `*_ID` constants above) plus the preset
/// itself. Ids match the desktop UI's preset ids (04 §S6) and are the contract other crates
/// resolve against via [`Preset::by_id`].
#[derive(Debug, Clone, PartialEq)]
pub struct ShippedPreset {
    pub id: &'static str,
    pub preset: Preset,
}

impl Preset {
    /// The seven shipped presets (03 §4.9, 04 §S6), in UI list order. Built fresh on every
    /// call (cheap: seven small structs) rather than cached, so this stays a plain function
    /// with no lazy-static machinery.
    pub fn shipped() -> Vec<ShippedPreset> {
        vec![
            ShippedPreset {
                id: PODCAST_STEREO_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Podcast (Stereo −16)".into(),
                    tier: Tier::Standard,
                    target_lufs: -16.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
            },
            ShippedPreset {
                id: PODCAST_MONO_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Podcast (Mono −19)".into(),
                    tier: Tier::Standard,
                    target_lufs: -19.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
            },
            ShippedPreset {
                id: SPOTIFY_YOUTUBE_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Spotify/YouTube −14".into(),
                    tier: Tier::Standard,
                    target_lufs: -14.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
            },
            ShippedPreset {
                id: BROADCAST_EBU_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Broadcast EBU −23".into(),
                    tier: Tier::Standard,
                    target_lufs: -23.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
            },
            ShippedPreset {
                id: AUDIOBOOK_ACX_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Audiobook (ACX)".into(),
                    tier: Tier::Standard,
                    // ACX grades on an RMS window (−23..−18 dBFS, 03 §4.9), not LUFS; −20.5
                    // sits at the window's midpoint and is what the two-pass normalize uses
                    // as its target. The compliance report runs the real ACX RMS/peak/floor
                    // checks (`ComplianceInput::acx_checks`) rather than relying on this.
                    target_lufs: -20.5,
                    true_peak_ceiling_dbtp: ACX_TRUE_PEAK_CEILING_DBTP,
                },
            },
            ShippedPreset {
                id: VOICE_MEMO_CLEANUP_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Voice memo cleanup".into(),
                    // Fast tier: single-voice, casual source, lowest latency (03 §7).
                    tier: Tier::Fast,
                    target_lufs: -16.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
            },
            ShippedPreset {
                id: MUSIC_HEAVY_SHOW_ID,
                preset: Preset {
                    schema_version: PROJECT_SCHEMA_VERSION,
                    name: "Music-heavy show".into(),
                    // Fast tier: lighter denoise so music beds aren't chewed up by the full
                    // DeepFilterNet3 chain (03 §2 music-majority rule).
                    tier: Tier::Fast,
                    target_lufs: -14.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
            },
        ]
    }

    /// Resolve a shipped preset by its stable id (e.g. `"podcast_stereo"`, see the `*_ID`
    /// constants). `None` if `id` isn't shipped — custom presets are addressed by their
    /// `.anvilpreset` file, not by id.
    pub fn by_id(id: &str) -> Option<Preset> {
        Preset::shipped()
            .into_iter()
            .find(|shipped| shipped.id == id)
            .map(|shipped| shipped.preset)
    }
}

/// Conventional file extension for a shareable preset document.
pub const PRESET_FILE_EXTENSION: &str = "anvilpreset";

/// A shareable `.anvilpreset` document (04 §S6 "import/export"): the preset (target/tier)
/// plus any chain-parameter deltas from the default chain, as one JSON file a user can send
/// to a co-host, drop in a watch folder's config, or commit alongside a show's assets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnvilPreset {
    pub schema_version: u32,
    /// The shipped preset id this was exported from, if any (a [`Preset::by_id`] key).
    /// `None` for a fully custom preset built from scratch. Round-trips through save/load so
    /// re-importing a shipped preset's export can still be recognized as that preset (e.g.
    /// by the compliance report's ACX detection).
    #[serde(default)]
    pub id: Option<String>,
    pub preset: Preset,
    /// Per-module parameter overrides from chain defaults (03 §4), keyed by module name
    /// (e.g. `"de_esser"`, `"autoeq"`). Kept as opaque JSON rather than typed chain-parameter
    /// structs so this crate doesn't need to depend on anvil-dsp: the DSP chain owns the
    /// meaning of each blob, this crate only owns the file format. `BTreeMap` for
    /// deterministic key order on serialize (stable diffs when a preset is committed to a
    /// repo).
    #[serde(default)]
    pub chain_deltas: BTreeMap<String, serde_json::Value>,
}

impl AnvilPreset {
    /// Wrap `preset` with no chain deltas and no shipped-id provenance (a from-scratch
    /// custom preset).
    pub fn new(preset: Preset) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            id: None,
            preset,
            chain_deltas: BTreeMap::new(),
        }
    }

    /// Export a shipped preset by id (see the `*_ID` constants), tagging it with that id for
    /// provenance. `None` if `id` isn't shipped.
    pub fn from_shipped(id: &str) -> Option<Self> {
        Preset::by_id(id).map(|preset| Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            id: Some(id.to_string()),
            preset,
            chain_deltas: BTreeMap::new(),
        })
    }

    /// Write this preset to `path` (conventionally named `<name>.anvilpreset`).
    /// Write-temp-then-rename for crash safety, matching [`crate::Project::save`] and
    /// [`crate::Settings::save`] (ADR-008 "crash-safe via write-temp-then-rename").
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension(format!("{PRESET_FILE_EXTENSION}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read an `.anvilpreset` file from `path`, migrating forward first if it was written by
    /// an older build (see [`migrate_anvilpreset`]). A newer-than-supported schema version is
    /// a hard error rather than silently dropping fields, matching [`crate::Project::load`]
    /// (ADR-008 "code must support at least N-1 schema version").
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
        migrate_anvilpreset(raw)
    }
}

/// Migration hook keyed off `schema_version`, mirroring
/// [`crate::project::migrate_manifest`]. No migrations exist yet — v1 is the only version —
/// so this is currently a passthrough for `version <= PROJECT_SCHEMA_VERSION`.
fn migrate_anvilpreset(raw: serde_json::Value) -> Result<AnvilPreset> {
    let found = raw
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;

    if found > PROJECT_SCHEMA_VERSION {
        return Err(Error::UnsupportedSchemaVersion {
            found,
            supported: PROJECT_SCHEMA_VERSION,
        });
    }

    // Migration stub: future schema bumps add `if found < N { ... }` steps here, each
    // rewriting `raw` forward one version, before falling through to the deserialize.
    let migrated = raw;

    Ok(serde_json::from_value(migrated)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SHIPPED_IDS: [&str; 7] = [
        PODCAST_STEREO_ID,
        PODCAST_MONO_ID,
        SPOTIFY_YOUTUBE_ID,
        BROADCAST_EBU_ID,
        AUDIOBOOK_ACX_ID,
        VOICE_MEMO_CLEANUP_ID,
        MUSIC_HEAVY_SHOW_ID,
    ];

    #[test]
    fn shipped_has_seven_presets_with_unique_ids() {
        let shipped = Preset::shipped();
        assert_eq!(shipped.len(), 7);
        let mut ids: Vec<&str> = shipped.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 7, "shipped preset ids must be unique");
    }

    #[test]
    fn all_documented_ids_resolve() {
        for id in ALL_SHIPPED_IDS {
            assert!(Preset::by_id(id).is_some(), "{id} should resolve");
        }
    }

    #[test]
    fn by_id_unknown_returns_none() {
        assert!(Preset::by_id("does_not_exist").is_none());
    }

    #[test]
    fn podcast_stereo_matches_default_targets() {
        // The shipped registry's podcast_stereo entry must agree with `Preset::default`
        // (both are meant to be the same preset, just reached two different ways).
        let default = Preset::default();
        let shipped = Preset::by_id(PODCAST_STEREO_ID).unwrap();
        assert_eq!(shipped.target_lufs, default.target_lufs);
        assert_eq!(shipped.tier, default.tier);
        assert_eq!(
            shipped.true_peak_ceiling_dbtp,
            default.true_peak_ceiling_dbtp
        );
    }

    #[test]
    fn ids_resolve_to_expected_targets_tiers_and_ceilings() {
        let cases: [(&str, f32, Tier, f32); 7] = [
            (PODCAST_STEREO_ID, -16.0, Tier::Standard, -1.0),
            (PODCAST_MONO_ID, -19.0, Tier::Standard, -1.0),
            (SPOTIFY_YOUTUBE_ID, -14.0, Tier::Standard, -1.0),
            (BROADCAST_EBU_ID, -23.0, Tier::Standard, -1.0),
            (AUDIOBOOK_ACX_ID, -20.5, Tier::Standard, -3.0),
            (VOICE_MEMO_CLEANUP_ID, -16.0, Tier::Fast, -1.0),
            (MUSIC_HEAVY_SHOW_ID, -14.0, Tier::Fast, -1.0),
        ];
        for (id, target_lufs, tier, ceiling) in cases {
            let preset = Preset::by_id(id).unwrap_or_else(|| panic!("{id} should resolve"));
            assert_eq!(preset.target_lufs, target_lufs, "{id} target_lufs");
            assert_eq!(preset.tier, tier, "{id} tier");
            assert_eq!(
                preset.true_peak_ceiling_dbtp, ceiling,
                "{id} true_peak_ceiling_dbtp"
            );
        }
    }

    #[test]
    fn acx_ceiling_is_tighter_than_default() {
        let acx = Preset::by_id(AUDIOBOOK_ACX_ID).unwrap();
        assert_eq!(acx.true_peak_ceiling_dbtp, -3.0);
    }

    #[test]
    fn anvilpreset_roundtrips_json() {
        let doc = AnvilPreset::new(Preset::default());
        let json = serde_json::to_string_pretty(&doc).unwrap();
        let back: AnvilPreset = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn anvilpreset_roundtrips_through_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("my-show.anvilpreset");

        let mut doc = AnvilPreset::from_shipped(AUDIOBOOK_ACX_ID).unwrap();
        doc.chain_deltas.insert(
            "de_esser".to_string(),
            serde_json::json!({ "threshold_db": -18.5 }),
        );

        doc.save(&path).unwrap();
        let loaded = AnvilPreset::load(&path).unwrap();

        assert_eq!(loaded, doc);
        assert_eq!(loaded.id.as_deref(), Some(AUDIOBOOK_ACX_ID));
    }

    #[test]
    fn anvilpreset_save_leaves_no_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("custom.anvilpreset");
        AnvilPreset::new(Preset::default()).save(&path).unwrap();
        assert!(path.exists());
        assert!(!tmp.path().join("custom.anvilpreset.tmp").exists());
    }

    #[test]
    fn anvilpreset_save_creates_missing_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("custom.anvilpreset");
        assert!(!path.exists());
        AnvilPreset::new(Preset::default()).save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn anvilpreset_load_rejects_newer_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("future.anvilpreset");

        let mut raw = serde_json::to_value(AnvilPreset::new(Preset::default())).unwrap();
        raw["schema_version"] = serde_json::json!(PROJECT_SCHEMA_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let err = AnvilPreset::load(&path).unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn anvilpreset_custom_preset_has_no_id() {
        let doc = AnvilPreset::new(Preset::default());
        assert_eq!(doc.id, None);
    }
}
