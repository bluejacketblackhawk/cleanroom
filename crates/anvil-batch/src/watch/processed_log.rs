//! Marker sidecar for "never re-process outputs" (04 §S5).
//!
//! Two independent guards make outputs safe from ever re-triggering their own rule:
//! 1. **Output-dir exclusion** — [`super::WatchService`] never treats a path under a
//!    rule's `output_dir` as a candidate input, so a rendered file (or this log itself)
//!    can't be picked back up even if `output_dir` sits inside (or overlaps) the watched
//!    folder.
//! 2. **This log** — a small JSON sidecar recording which inputs (by path + size +
//!    modified-time fingerprint) a rule has already rendered, persisted in the rule's
//!    `output_dir`. It survives a service restart, so touching/re-copying a file that
//!    was already processed doesn't queue it again just because `notify` re-announces it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the sidecar, written inside a watch rule's output directory.
pub const PROCESSED_LOG_FILE: &str = ".anvil-watch-log.json";

/// A (size, modified-time-in-ms) fingerprint identifying a specific version of a file.
pub type Fingerprint = (u64, u64);

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProcessedLog {
    #[serde(default)]
    entries: HashMap<String, Fingerprint>,
}

impl ProcessedLog {
    fn sidecar_path(output_dir: &Path) -> PathBuf {
        output_dir.join(PROCESSED_LOG_FILE)
    }

    /// Load a rule's processed-inputs log from its output directory. Missing or
    /// unreadable/corrupt logs yield an empty log rather than an error — losing the
    /// dedup history in a worst case just costs a re-render, never a crash.
    pub fn load(output_dir: &Path) -> Self {
        std::fs::read(Self::sidecar_path(output_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persist the log back to `output_dir`. Best-effort: a failure here (e.g. the
    /// output volume just went away) doesn't fail the render that triggered it, but does
    /// mean that render might be repeated on the next restart.
    pub fn save(&self, output_dir: &Path) {
        if std::fs::create_dir_all(output_dir).is_err() {
            return;
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(Self::sidecar_path(output_dir), bytes);
        }
    }

    pub fn already_processed(&self, input: &Path, fingerprint: Fingerprint) -> bool {
        self.entries.get(&key(input)) == Some(&fingerprint)
    }

    pub fn mark_processed(&mut self, input: &Path, fingerprint: Fingerprint) {
        self.entries.insert(key(input), fingerprint);
    }
}

fn key(input: &Path) -> String {
    input.to_string_lossy().into_owned()
}

/// This sidecar's own file name is never a valid batch/watch input regardless of its
/// extension, so a rule watching its own output directory can't ingest its own log.
pub fn is_log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == PROCESSED_LOG_FILE)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("ep1.wav");

        let mut log = ProcessedLog::load(tmp.path());
        assert!(!log.already_processed(&input, (100, 200)));

        log.mark_processed(&input, (100, 200));
        log.save(tmp.path());

        let reloaded = ProcessedLog::load(tmp.path());
        assert!(reloaded.already_processed(&input, (100, 200)));
        // A different fingerprint for the same path (file changed since) is not
        // considered already-processed.
        assert!(!reloaded.already_processed(&input, (999, 200)));
    }

    #[test]
    fn missing_log_loads_empty_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ProcessedLog::load(&tmp.path().join("nonexistent"));
        assert!(!log.already_processed(Path::new("anything"), (0, 0)));
    }

    #[test]
    fn recognizes_its_own_sidecar_file_name() {
        assert!(is_log_file(Path::new("/out/.anvil-watch-log.json")));
        assert!(!is_log_file(Path::new("/out/ep1_mastered.wav")));
    }
}
