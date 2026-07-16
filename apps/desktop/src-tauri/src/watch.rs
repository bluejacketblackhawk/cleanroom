//! Watch-folder rule commands (04 §S5): thin wrappers over `anvil_batch::WatchService`,
//! sharing the same `BatchQueue` the Batch screen drives — watch-triggered and manually
//! submitted jobs show up side by side in one queue/table. Rule status streams to the UI
//! via a polling thread (`watch://status`, see `lib.rs::spawn_progress_poller`).

use std::path::PathBuf;
use std::sync::Arc;

use anvil_batch::{BatchQueue, FilePattern, WatchRule, WatchRuleId, WatchRuleStatus, WatchService};
use tauri::State;

use crate::presets::{self, PresetsState};

pub struct WatchState {
    pub service: WatchService,
}

impl WatchState {
    /// Shares `queue` with the Batch screen (04 §S5 module docs: "the same queue a UI's
    /// Batch screen uses").
    pub fn new(queue: Arc<BatchQueue>) -> Self {
        Self {
            service: WatchService::new(queue),
        }
    }
}

/// Every rule's current status, for the S5 rule list.
#[tauri::command]
pub fn watch_list_rules(watch: State<'_, WatchState>) -> Vec<WatchRuleStatus> {
    watch.service.list_rules()
}

/// Add a rule: folder → preset → output dir → pattern → on (04 §S5). `extensions`, if
/// non-empty, restricts the pattern filter; omitted or empty accepts anything
/// `anvil_batch` recognizes.
#[tauri::command]
pub fn watch_add_rule(
    folder: String,
    preset_ref: String,
    tier: String,
    output_dir: String,
    extensions: Option<Vec<String>>,
    watch: State<'_, WatchState>,
    presets: State<'_, PresetsState>,
) -> Result<WatchRuleId, String> {
    let preset = presets::resolve_preset_ref(&preset_ref, presets.dir())?;
    let tier = presets::parse_tier(&tier);
    let mut rule = WatchRule::new(
        PathBuf::from(folder),
        preset,
        tier,
        PathBuf::from(output_dir),
    );
    if let Some(exts) = extensions {
        if !exts.is_empty() {
            rule.pattern = FilePattern::Extensions(exts);
        }
    }
    Ok(watch.service.add_rule(rule))
}

#[tauri::command]
pub fn watch_remove_rule(id: WatchRuleId, watch: State<'_, WatchState>) -> bool {
    watch.service.remove_rule(id)
}

/// Turn a rule on (starts its watcher) or off (04 §S5 "add/remove/enable").
#[tauri::command]
pub fn watch_set_enabled(id: WatchRuleId, enabled: bool, watch: State<'_, WatchState>) -> bool {
    watch.service.set_enabled(id, enabled)
}

/// Retry every rule currently showing "watch folder unreachable" (e.g. a network share
/// that just came back online).
#[tauri::command]
pub fn watch_retry_unreachable(watch: State<'_, WatchState>) {
    watch.service.retry_unreachable();
}
