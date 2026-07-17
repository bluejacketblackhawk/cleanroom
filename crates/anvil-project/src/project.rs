//! The `.anvilproj` project folder: a schema-versioned `project.json` manifest bundling
//! sources, preset, EDL, and placeholders for render history / analysis cache (ADR-008).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use anvil_core::{Error, Result};

use crate::{Edl, Preset, PROJECT_SCHEMA_VERSION};

/// File name of the manifest inside an `.anvilproj` folder.
pub const PROJECT_MANIFEST_FILE: &str = "project.json";

/// Render-history log: one entry per completed render. Populated starting in a later
/// milestone (M1+); the shape here just reserves the slot in the schema so `project.json`
/// doesn't need a breaking migration to grow it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RenderHistory {
    pub entries: Vec<serde_json::Value>,
}

/// Analysis-cache manifest: pointers to cached VAD/loudness/classifier results per
/// source file (the `analysis/` folder, ADR-008). Populated starting in a later
/// milestone; reserved here for the same forward-compat reason as [`RenderHistory`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnalysisCache {
    pub entries: Vec<serde_json::Value>,
}

/// The on-disk project manifest: `project.json` inside the `.anvilproj` folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub schema_version: u32,
    pub name: String,
    pub preset: Preset,
    pub edl: Edl,
    #[serde(default)]
    pub render_history: RenderHistory,
    #[serde(default)]
    pub analysis_cache: AnalysisCache,
}

impl ProjectManifest {
    /// A fresh manifest at the current schema version: default preset, empty EDL, empty
    /// history/cache.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            name: name.into(),
            preset: Preset::default(),
            edl: Edl::default(),
            render_history: RenderHistory::default(),
            analysis_cache: AnalysisCache::default(),
        }
    }
}

/// An open project: the manifest plus the folder it lives in (once saved/loaded).
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub manifest: ProjectManifest,
}

impl Project {
    /// A fresh, unsaved project.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            manifest: ProjectManifest::new(name),
        }
    }

    /// Write the project to `dir` (an `.anvilproj` folder), creating it if needed.
    /// The manifest is written via write-temp-then-rename so a crash or power loss
    /// mid-write can never leave `project.json` truncated or corrupt (ADR-008
    /// "Crash-safe via write-temp-then-rename").
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec_pretty(&self.manifest)?;
        let final_path = dir.join(PROJECT_MANIFEST_FILE);
        let tmp_path = dir.join(format!("{PROJECT_MANIFEST_FILE}.tmp"));
        std::fs::write(&tmp_path, bytes)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Read a project from `dir`. If the on-disk schema is older than
    /// [`PROJECT_SCHEMA_VERSION`], it's migrated forward first (currently a no-op stub —
    /// v1 is the first schema version, see [`migrate_manifest`]); if it's newer than this
    /// build understands, loading fails loudly with [`Error::UnsupportedSchemaVersion`]
    /// rather than silently dropping fields (ADR-008 "code must support at least N-1
    /// schema version").
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(PROJECT_MANIFEST_FILE);
        let bytes = std::fs::read(&path)?;
        let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
        let manifest = migrate_manifest(raw)?;
        Ok(Self { manifest })
    }
}

/// Migration hook keyed off `schema_version`. A newer-than-supported version is a hard
/// error (the user needs a newer Cleanroom build); an older version is migrated forward
/// before final deserialization. No migrations exist yet — v1 is the only version — so
/// this is currently a passthrough for `version <= PROJECT_SCHEMA_VERSION`.
fn migrate_manifest(raw: serde_json::Value) -> Result<ProjectManifest> {
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

/// Convenience alias kept alongside [`PROJECT_MANIFEST_FILE`] for callers that want the
/// conventional `.anvilproj` extension when naming a new project folder.
pub const PROJECT_DIR_EXTENSION: &str = "anvilproj";

/// Build a conventional `.anvilproj` folder path for `name` under `parent`.
pub fn project_dir_path(parent: &Path, name: &str) -> PathBuf {
    parent.join(format!("{name}.{PROJECT_DIR_EXTENSION}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("show.anvilproj");

        let mut project = Project::new("show");
        project
            .manifest
            .edl
            .sources
            .push(crate::EdlSource::new("ep1.wav"));
        project
            .manifest
            .edl
            .segments
            .push(crate::Segment::kept(0, 0.0, 30.0));

        project.save(&dir).unwrap();
        let loaded = Project::load(&dir).unwrap();

        assert_eq!(loaded.manifest, project.manifest);
    }

    #[test]
    fn save_creates_missing_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("show.anvilproj");
        assert!(!dir.exists());

        Project::new("show").save(&dir).unwrap();

        assert!(dir.join(PROJECT_MANIFEST_FILE).exists());
    }

    #[test]
    fn load_rejects_newer_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("show.anvilproj");
        std::fs::create_dir_all(&dir).unwrap();

        let mut manifest = serde_json::to_value(ProjectManifest::new("show")).unwrap();
        manifest["schema_version"] = serde_json::json!(PROJECT_SCHEMA_VERSION + 1);
        std::fs::write(
            dir.join(PROJECT_MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let err = Project::load(&dir).unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn no_tmp_file_left_behind_after_save() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("show.anvilproj");
        Project::new("show").save(&dir).unwrap();
        assert!(!dir.join(format!("{PROJECT_MANIFEST_FILE}.tmp")).exists());
    }
}
