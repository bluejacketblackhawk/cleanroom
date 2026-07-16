//! Presets manager backend (04 §S6): the seven shipped presets (data, from
//! `anvil_project::preset`) plus a per-user `.anvilpreset` library at
//! `<config_dir>/presets/<uuid>.anvilpreset`.
//!
//! `preset_ref` strings are the wire contract every preset-consuming command shares
//! (`master`, batch submit, watch rules): a bare shipped id (`"podcast_stereo"`, see
//! `anvil_project::preset::*_ID`) or `"user:<uuid>"` for a preset in the user library.
//! [`resolve_preset_ref`] is the one place that contract is decoded.

use std::path::{Path, PathBuf};

use anvil_core::platform::Platform;
use anvil_project::{AnvilPreset, Preset, Tier, PRESET_FILE_EXTENSION};
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

/// Where the user's own presets live. Created lazily on first write, not at startup, so
/// a user who never touches Presets never gets an empty folder on disk.
pub struct PresetsState {
    dir: PathBuf,
}

impl PresetsState {
    pub fn new() -> Self {
        let dir = anvil_core::platform::current().config_dir().join("presets");
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Default for PresetsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the on-disk path for user preset `id` — but only once `id` is confirmed to be a
/// real UUID (every id this module itself ever hands out, via `Uuid::new_v4()`). Without
/// this check, an attacker-controlled `preset_ref` like `"user:../../../whatever"` would
/// `dir.join()` straight past the presets folder: `resolve_preset_ref` (read, used by
/// master/batch/watch), `presets_update` (write), and `presets_delete` (delete!) would all
/// operate on an arbitrary path outside `<config_dir>/presets`. Rejecting anything that
/// doesn't parse as a UUID closes that off at the one place every caller routes through.
fn user_path(dir: &Path, id: &str) -> Result<PathBuf, String> {
    let id = Uuid::parse_str(id).map_err(|_| "invalid preset id".to_string())?;
    Ok(dir.join(format!("{id}.{PRESET_FILE_EXTENSION}")))
}

/// Decode a `preset_ref` (see module docs) into a resolved [`Preset`]. The one place
/// `master`/batch/watch commands turn a UI selection into engine input.
pub fn resolve_preset_ref(preset_ref: &str, dir: &Path) -> Result<Preset, String> {
    if let Some(id) = preset_ref.strip_prefix("user:") {
        let doc = AnvilPreset::load(&user_path(dir, id)?).map_err(|e| e.to_string())?;
        return Ok(doc.preset);
    }
    Preset::by_id(preset_ref).ok_or_else(|| format!("unknown preset: {preset_ref}"))
}

/// Parse a UI tier string ("fast" | "standard" | "studio"); unrecognized values fall
/// back to Standard, matching the M1 `master` command's existing behavior.
pub fn parse_tier(s: &str) -> Tier {
    match s {
        "fast" => Tier::Fast,
        "studio" => Tier::Studio,
        _ => Tier::Standard,
    }
}

fn tier_str(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Standard => "standard",
        Tier::Studio => "studio",
    }
}

/// One row in the S6 preset card grid.
#[derive(Debug, Clone, Serialize)]
pub struct PresetSummary {
    /// The wire id `master`/batch/watch accept.
    pub preset_ref: String,
    pub name: String,
    pub tier: String,
    pub target_lufs: f32,
    pub true_peak_ceiling_dbtp: f32,
    /// "shipped" presets are read-only in the UI (duplicate to customize); "user" ones
    /// are the editable/duplicable/deletable library.
    pub source: &'static str,
}

fn summarize(preset_ref: String, preset: &Preset, source: &'static str) -> PresetSummary {
    PresetSummary {
        preset_ref,
        name: preset.name.clone(),
        tier: tier_str(preset.tier).to_string(),
        target_lufs: preset.target_lufs,
        true_peak_ceiling_dbtp: preset.true_peak_ceiling_dbtp,
        source,
    }
}

/// Every preset the UI can offer: the seven shipped ones in catalog order, then the
/// user's own library sorted by name.
#[tauri::command]
pub fn presets_list(state: State<'_, PresetsState>) -> Vec<PresetSummary> {
    let mut out: Vec<PresetSummary> = Preset::shipped()
        .into_iter()
        .map(|shipped| summarize(shipped.id.to_string(), &shipped.preset, "shipped"))
        .collect();

    if let Ok(entries) = std::fs::read_dir(state.dir()) {
        let mut user: Vec<PresetSummary> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().and_then(|x| x.to_str()) == Some(PRESET_FILE_EXTENSION)
            })
            .filter_map(|e| {
                let id = e.path().file_stem()?.to_str()?.to_string();
                let doc = AnvilPreset::load(&e.path()).ok()?;
                Some(summarize(format!("user:{id}"), &doc.preset, "user"))
            })
            .collect();
        user.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(user);
    }
    out
}

/// Copy a shipped or user preset into the user library under a new name, ready to edit
/// (04 §S6 "duplicate/edit" — shipped presets themselves are never mutated).
#[tauri::command]
pub fn presets_duplicate(
    preset_ref: String,
    new_name: String,
    state: State<'_, PresetsState>,
) -> Result<PresetSummary, String> {
    let mut preset = resolve_preset_ref(&preset_ref, state.dir())?;
    preset.name = new_name;
    let id = Uuid::new_v4().to_string();
    std::fs::create_dir_all(state.dir()).map_err(|e| e.to_string())?;
    AnvilPreset::new(preset.clone())
        .save(&user_path(state.dir(), &id)?)
        .map_err(|e| e.to_string())?;
    Ok(summarize(format!("user:{id}"), &preset, "user"))
}

/// Editable fields for a user preset (04 §S6 "edit"). Chain-parameter deltas, if the
/// preset has any, are left untouched.
#[derive(Debug, Clone, Deserialize)]
pub struct PresetEdit {
    pub name: String,
    pub tier: String,
    pub target_lufs: f32,
    pub true_peak_ceiling_dbtp: f32,
}

#[tauri::command]
pub fn presets_update(
    preset_ref: String,
    edit: PresetEdit,
    state: State<'_, PresetsState>,
) -> Result<PresetSummary, String> {
    let id = preset_ref.strip_prefix("user:").ok_or_else(|| {
        "only your own presets can be edited — duplicate this one first".to_string()
    })?;
    let path = user_path(state.dir(), id)?;
    let mut doc = AnvilPreset::load(&path).map_err(|e| e.to_string())?;
    doc.preset.name = edit.name;
    doc.preset.tier = parse_tier(&edit.tier);
    doc.preset.target_lufs = edit.target_lufs;
    doc.preset.true_peak_ceiling_dbtp = edit.true_peak_ceiling_dbtp;
    doc.save(&path).map_err(|e| e.to_string())?;
    Ok(summarize(preset_ref, &doc.preset, "user"))
}

#[tauri::command]
pub fn presets_delete(preset_ref: String, state: State<'_, PresetsState>) -> Result<(), String> {
    let id = preset_ref
        .strip_prefix("user:")
        .ok_or_else(|| "only your own presets can be deleted".to_string())?;
    std::fs::remove_file(user_path(state.dir(), id)?).map_err(|e| e.to_string())
}

/// Import an `.anvilpreset` file from anywhere on disk into the user library.
#[tauri::command]
pub fn presets_import(
    path: String,
    state: State<'_, PresetsState>,
) -> Result<PresetSummary, String> {
    let doc = AnvilPreset::load(Path::new(&path)).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().to_string();
    std::fs::create_dir_all(state.dir()).map_err(|e| e.to_string())?;
    doc.save(&user_path(state.dir(), &id)?)
        .map_err(|e| e.to_string())?;
    Ok(summarize(format!("user:{id}"), &doc.preset, "user"))
}

/// Export a shipped or user preset to an arbitrary destination (04 §S6 "import/export").
#[tauri::command]
pub fn presets_export(
    preset_ref: String,
    dest_path: String,
    state: State<'_, PresetsState>,
) -> Result<(), String> {
    let doc = if let Some(id) = preset_ref.strip_prefix("user:") {
        AnvilPreset::load(&user_path(state.dir(), id)?).map_err(|e| e.to_string())?
    } else {
        AnvilPreset::from_shipped(&preset_ref)
            .ok_or_else(|| format!("unknown preset: {preset_ref}"))?
    };
    doc.save(Path::new(&dest_path)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_preset_ref_finds_shipped_ids() {
        let p = resolve_preset_ref("podcast_stereo", Path::new(".")).unwrap();
        assert_eq!(p.target_lufs, -16.0);
    }

    #[test]
    fn resolve_preset_ref_errors_on_unknown_shipped_id() {
        let err = resolve_preset_ref("does_not_exist", Path::new(".")).unwrap_err();
        assert!(err.contains("does_not_exist"));
    }

    /// A `preset_ref` isn't a trusted value — it round-trips through the UI/IPC, so a
    /// non-UUID `user:<...>` id (deliberately crafted or just corrupted state) must never
    /// reach `Path::join` unchecked. Every `user:` caller (`resolve_preset_ref` here;
    /// `presets_update`/`presets_delete`/`presets_export` share the same `user_path`) is
    /// covered by this one check.
    #[test]
    fn resolve_preset_ref_rejects_a_traversing_user_id() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_preset_ref("user:../../../../whatever", tmp.path()).unwrap_err();
        assert_eq!(err, "invalid preset id");
    }

    #[test]
    fn user_presets_roundtrip_duplicate_update_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        let mut preset = Preset::by_id("podcast_stereo").unwrap();
        preset.name = "My Show".into();
        let id = Uuid::new_v4().to_string();
        AnvilPreset::new(preset.clone())
            .save(&user_path(&dir, &id).unwrap())
            .unwrap();

        let preset_ref = format!("user:{id}");
        let resolved = resolve_preset_ref(&preset_ref, &dir).unwrap();
        assert_eq!(resolved.name, "My Show");

        assert!(user_path(&dir, &id).unwrap().exists());
        std::fs::remove_file(user_path(&dir, &id).unwrap()).unwrap();
        assert!(resolve_preset_ref(&preset_ref, &dir).is_err());
    }
}
