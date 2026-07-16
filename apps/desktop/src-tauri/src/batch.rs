//! Batch queue commands (04 §S4): thin wrappers over `anvil_batch::BatchQueue`. Commands
//! themselves stay synchronous request/response (submit, cancel, pause, …); live progress
//! streams to the UI separately via a polling thread (`batch://progress`, see
//! `lib.rs::spawn_progress_poller`) so the table updates without the UI polling itself.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anvil_batch::{BatchItemState, BatchItemStatus, BatchJobId, BatchQueue, OutputSettings};
use anvil_core::platform::Platform;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::presets::{self, PresetsState};

pub struct BatchState {
    pub queue: Arc<BatchQueue>,
}

impl BatchState {
    /// Concurrency auto-scaled to N-1 cores (04 §S4 "concurrency auto").
    pub fn new() -> anvil_core::Result<Self> {
        Ok(Self {
            queue: Arc::new(BatchQueue::new()?),
        })
    }
}

// ---- crash recovery (05 §M5.F) ----------------------------------------------------------
//
// The batch queue lives in memory only — if the app is killed mid-batch (crash, forced
// quit, power loss), every item still `Queued`/`Running` at that moment is simply gone on
// next launch, with no partial output and no record it was ever requested. That's a silent
// loss of the user's work, not just a stalled progress bar. This ledger is a small, boring
// fix: a JSON file of "in-flight" items in the platform config dir, appended to on submit
// and pruned to just the still-live ids on every progress-poller tick (`lib.rs`'s
// `spawn_progress_poller`). A leftover file at startup means the last session didn't drain
// cleanly; `batch_check_recovery` surfaces it to the UI so the user can decide to re-drop
// those files — nothing here ever auto-resubmits a job from a file the user didn't act on
// (same "never take an action from silence" rule the rest of the queue follows).

/// One ledger entry: enough for the UI to tell the user what didn't finish and let them
/// re-drop it. Not a full [`anvil_batch::BatchJob`] — the queue is the source of truth for
/// preset/tier while a job is live; once it's gone from the queue, only the file paths
/// still mean anything to show.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryEntry {
    pub input: PathBuf,
    pub output: PathBuf,
}

fn recovery_path() -> PathBuf {
    anvil_core::platform::current()
        .config_dir()
        .join("batch-recovery.json")
}

// `_at` split (same convention as `anvil_core::platform::windows`'s registry tests): the
// real path is a fixed OS location, so unit tests exercise these against a throwaway temp
// path instead — never the real per-user config dir.

fn read_recovery_at(path: &Path) -> Vec<RecoveryEntry> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_recovery_at(path: &Path, entries: &[RecoveryEntry]) {
    if entries.is_empty() {
        // Nothing left in flight — remove the file rather than leave an empty one behind,
        // so a clean drain (the common case) never nags the next launch.
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(entries) {
        let _ = std::fs::write(path, json);
    }
}

fn read_recovery() -> Vec<RecoveryEntry> {
    read_recovery_at(&recovery_path())
}

fn write_recovery(entries: &[RecoveryEntry]) {
    write_recovery_at(&recovery_path(), entries)
}

/// Append the just-submitted jobs to the ledger, reading their resolved output paths back
/// from the queue's own snapshot rather than recomputing the naming-pattern/back-catalog
/// logic here a second time (that logic is `anvil_batch::catalog`'s job, not this file's).
fn record_submitted(batch: &BatchQueue, ids: &[BatchJobId]) {
    if ids.is_empty() {
        return;
    }
    let id_set: HashSet<BatchJobId> = ids.iter().copied().collect();
    let mut entries = read_recovery();
    for status in batch.snapshot() {
        if id_set.contains(&status.id) {
            entries.push(RecoveryEntry {
                input: status.input,
                output: status.output,
            });
        }
    }
    write_recovery(&entries);
}

/// Drop every ledger entry whose job has since left the `Queued`/`Running` states — it
/// finished, failed, or was cancelled, all of which the UI already reflects live, so
/// there's nothing left to "recover" after a crash. Called from the progress poller
/// (`lib.rs::spawn_progress_poller`) on the same tick as the `batch://progress` broadcast.
pub fn prune_recovery(batch: &BatchQueue) {
    let path = recovery_path();
    if !path.exists() {
        return;
    }
    let live: HashSet<PathBuf> = batch
        .snapshot()
        .into_iter()
        .filter(|s| matches!(s.state, BatchItemState::Queued | BatchItemState::Running))
        .map(|s| s.output)
        .collect();
    let entries = read_recovery();
    let kept: Vec<_> = entries
        .into_iter()
        .filter(|e| live.contains(&e.output))
        .collect();
    write_recovery(&kept);
}

/// Whatever the ledger says is still in flight from a previous session — called once at
/// startup (04's onboarding/empty-state pattern: surface it, never act on it silently).
#[tauri::command]
pub fn batch_check_recovery() -> Vec<RecoveryEntry> {
    read_recovery()
}

/// The user acknowledged the recovery banner (whether or not they re-dropped the files) —
/// clear the ledger so it doesn't resurface.
#[tauri::command]
pub fn batch_dismiss_recovery() {
    write_recovery(&[]);
}

/// Output settings from the S4 screen: where results land, whether back-catalog mode
/// mirrors subfolders, and the naming pattern (04 §S8 tokens — only `{name}` today).
#[derive(Debug, Clone, Deserialize)]
pub struct BatchOutputSpec {
    pub output_dir: String,
    #[serde(default)]
    pub preserve_structure: bool,
    #[serde(default)]
    pub naming: Option<String>,
}

fn build_output_settings(spec: &BatchOutputSpec) -> OutputSettings {
    let mut settings = OutputSettings::new(PathBuf::from(&spec.output_dir));
    if let Some(naming) = &spec.naming {
        settings.naming = naming.clone();
    }
    if spec.preserve_structure {
        settings = settings.preserving_structure();
    }
    settings
}

/// Submit a flat file list (04 §S4 "drop N files") under one preset/tier.
#[tauri::command]
pub fn batch_submit_files(
    inputs: Vec<String>,
    preset_ref: String,
    tier: String,
    output: BatchOutputSpec,
    batch: State<'_, BatchState>,
    presets: State<'_, PresetsState>,
) -> Result<Vec<BatchJobId>, String> {
    let preset = presets::resolve_preset_ref(&preset_ref, presets.dir())?;
    let tier = presets::parse_tier(&tier);
    let settings = build_output_settings(&output);
    let paths: Vec<PathBuf> = inputs.into_iter().map(PathBuf::from).collect();
    let ids = batch.queue.submit_files(paths, preset, tier, &settings);
    record_submitted(&batch.queue, &ids);
    Ok(ids)
}

/// Submit a folder (04 §S4 back-catalog mode), recursing and honoring
/// `output.preserve_structure`.
#[tauri::command]
pub fn batch_submit_folder(
    root: String,
    preset_ref: String,
    tier: String,
    output: BatchOutputSpec,
    batch: State<'_, BatchState>,
    presets: State<'_, PresetsState>,
) -> Result<Vec<BatchJobId>, String> {
    let preset = presets::resolve_preset_ref(&preset_ref, presets.dir())?;
    let tier = presets::parse_tier(&tier);
    let settings = build_output_settings(&output);
    let ids = batch
        .queue
        .submit_folder(Path::new(&root), preset, tier, &settings)
        .map_err(|e| e.to_string())?;
    record_submitted(&batch.queue, &ids);
    Ok(ids)
}

/// Full table snapshot (also used for the initial paint, before the first
/// `batch://progress` poll tick arrives).
#[tauri::command]
pub fn batch_snapshot(batch: State<'_, BatchState>) -> Vec<BatchItemStatus> {
    batch.queue.snapshot()
}

#[tauri::command]
pub fn batch_overall_progress(batch: State<'_, BatchState>) -> f32 {
    batch.queue.overall_progress()
}

#[tauri::command]
pub fn batch_cancel(id: BatchJobId, batch: State<'_, BatchState>) -> bool {
    batch.queue.cancel(id)
}

#[tauri::command]
pub fn batch_cancel_all(batch: State<'_, BatchState>) {
    batch.queue.cancel_all();
}

#[tauri::command]
pub fn batch_pause(batch: State<'_, BatchState>) {
    batch.queue.pause();
}

#[tauri::command]
pub fn batch_resume(batch: State<'_, BatchState>) {
    batch.queue.resume();
}

#[tauri::command]
pub fn batch_is_paused(batch: State<'_, BatchState>) -> bool {
    batch.queue.is_paused()
}

/// Move a still-pending item (04 §S4 "reorder, where practical" — running/finished items
/// return `false`).
#[tauri::command]
pub fn batch_reorder(id: BatchJobId, new_index: usize, batch: State<'_, BatchState>) -> bool {
    batch.queue.reorder(id, new_index)
}

#[tauri::command]
pub fn batch_remove(id: BatchJobId, batch: State<'_, BatchState>) -> bool {
    batch.queue.remove(id)
}

/// One-click retry-failed (04 §S4).
#[tauri::command]
pub fn batch_retry_failed(batch: State<'_, BatchState>) -> Vec<BatchJobId> {
    batch.queue.retry_failed()
}

/// Classify a dropped path as `"file"`, `"dir"`, or `"missing"` — lets the S4 drop zone
/// tell "drop N files" from "drop a folder" apart without a file-picker dialog (the OS
/// drag-drop payload is just paths, no type flag).
#[tauri::command]
pub fn batch_path_kind(path: String) -> &'static str {
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => "dir",
        Ok(_) => "file",
        Err(_) => "missing",
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn entry(n: u32) -> RecoveryEntry {
        RecoveryEntry {
            input: PathBuf::from(format!("in-{n}.wav")),
            output: PathBuf::from(format!("out-{n}.wav")),
        }
    }

    #[test]
    fn missing_ledger_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(read_recovery_at(&path).is_empty());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("batch-recovery.json");
        let entries = vec![entry(1), entry(2)];

        write_recovery_at(&path, &entries);
        let read_back = read_recovery_at(&path);

        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].input, PathBuf::from("in-1.wav"));
        assert_eq!(read_back[1].output, PathBuf::from("out-2.wav"));
    }

    #[test]
    fn writing_an_empty_ledger_removes_the_file() {
        // A clean drain (the common case) must not leave a stale/empty ledger nagging the
        // next launch.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-recovery.json");
        write_recovery_at(&path, &[entry(1)]);
        assert!(path.exists());

        write_recovery_at(&path, &[]);
        assert!(!path.exists());
        assert!(read_recovery_at(&path).is_empty());
    }
}
