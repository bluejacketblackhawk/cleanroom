//! Watch rule types (04 §S5): folder → preset → output dir → file-pattern filter →
//! on/off, plus the status snapshot a UI list renders.

use std::path::{Path, PathBuf};

use anvil_project::{Preset, Tier};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::catalog::is_supported;

/// Identifies one watch rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WatchRuleId(pub Uuid);

impl WatchRuleId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WatchRuleId {
    fn default() -> Self {
        Self::new()
    }
}

/// A file-pattern filter for a watch rule (04 §S5 "file-pattern filter").
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePattern {
    /// Accept anything `anvil_batch` knows how to pick up (`catalog::SUPPORTED_EXTENSIONS`).
    #[default]
    AnySupported,
    /// Accept only these extensions (case-insensitive, no leading dot — e.g. `["wav", "mp3"]`).
    Extensions(Vec<String>),
}

impl FilePattern {
    pub fn matches(&self, path: &Path) -> bool {
        match self {
            FilePattern::AnySupported => is_supported(path),
            FilePattern::Extensions(exts) => path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| exts.iter().any(|allowed| allowed.eq_ignore_ascii_case(e)))
                .unwrap_or(false),
        }
    }
}

/// folder → preset → output dir → pattern → on/off (04 §S5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchRule {
    #[serde(default = "WatchRuleId::new")]
    pub id: WatchRuleId,
    pub folder: PathBuf,
    pub preset: Preset,
    pub tier: Tier,
    pub output_dir: PathBuf,
    #[serde(default)]
    pub pattern: FilePattern,
    pub enabled: bool,
}

impl WatchRule {
    /// A new, enabled rule matching any supported file type.
    pub fn new(
        folder: impl Into<PathBuf>,
        preset: Preset,
        tier: Tier,
        output_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: WatchRuleId::new(),
            folder: folder.into(),
            preset,
            tier,
            output_dir: output_dir.into(),
            pattern: FilePattern::AnySupported,
            enabled: true,
        }
    }
}

/// A rule plus its live status, for a UI list (04 §S5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchRuleStatus {
    pub rule: WatchRule,
    /// Set when the watched folder can't be read (moved/unmounted/permissions) — the S5
    /// "watch folder unreachable (rule paused badge)" error state. `None` means healthy.
    pub error: Option<String>,
}
