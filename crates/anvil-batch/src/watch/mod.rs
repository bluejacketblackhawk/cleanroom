//! Watch-folder engine (04 §S5): rules (folder → preset → output dir → file-pattern
//! filter → on/off) driven by `notify`. New files are queued once they pass the
//! size-stable check (`stability`); outputs this service itself produces are never
//! re-picked-up as new inputs (`processed_log` + an in-memory "known outputs" set).
//!
//! UI-independent: a tray agent, the desktop app's S5 screen, or `anvil batch --watch`
//! (04 §CLI) all just drive this. Only *new* filesystem activity after a rule is added
//! is queued — adding a rule never bulk-processes files already sitting in the folder
//! (that's back-catalog mode, `catalog`/`BatchQueue::submit_folder`, a deliberate
//! separate action).

mod processed_log;
mod rule;
pub mod stability;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

pub use rule::{FilePattern, WatchRule, WatchRuleId, WatchRuleStatus};
pub use stability::{Clock, SystemClock};

use crate::catalog::{resolve_output, OutputSettings};
use crate::error::BatchError;
use crate::queue::{BatchJob, BatchQueue};
use processed_log::ProcessedLog;
use stability::StabilityTracker;

/// Runtime state for one active rule: its own `notify` watcher (if enabled and started
/// successfully), stability tracker, processed-inputs log, and the exact set of output
/// paths this service has produced for it (checked before any path is ever considered a
/// candidate, so a rendered file can never re-trigger its own rule no matter where
/// `output_dir` sits relative to the watched folder).
struct RuleRuntime {
    rule: WatchRule,
    watcher: Option<RecommendedWatcher>,
    tracker: StabilityTracker,
    processed: ProcessedLog,
    known_outputs: HashSet<PathBuf>,
    error: Option<String>,
}

struct ServiceState {
    rules: HashMap<WatchRuleId, RuleRuntime>,
}

struct Inner {
    queue: Arc<BatchQueue>,
    state: Mutex<ServiceState>,
    events_tx: crossbeam_channel::Sender<(WatchRuleId, PathBuf)>,
    stability_window: Duration,
    clock: Arc<dyn Clock>,
    shutdown: AtomicBool,
}

/// Drives watch rules: owns one `notify` watcher per enabled rule, funnels their events
/// through a per-rule stability check, and submits stabilized files to a shared
/// [`BatchQueue`] (the same queue a UI's Batch screen uses — watch-triggered and
/// manually-submitted jobs show up side by side).
pub struct WatchService {
    inner: Arc<Inner>,
    poller: Option<thread::JoinHandle<()>>,
}

impl WatchService {
    /// A service with the 04 §S5 defaults: a 5 s stability window, polled every 500 ms.
    pub fn new(queue: Arc<BatchQueue>) -> Self {
        Self::with_config(
            queue,
            Duration::from_secs(5),
            Duration::from_millis(500),
            Arc::new(SystemClock),
        )
    }

    /// Full control over timing and the clock source — tests use tiny windows/intervals
    /// and a fake clock so nothing here ever needs a real multi-second sleep.
    pub fn with_config(
        queue: Arc<BatchQueue>,
        stability_window: Duration,
        poll_interval: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let inner = Arc::new(Inner {
            queue,
            state: Mutex::new(ServiceState {
                rules: HashMap::new(),
            }),
            events_tx,
            stability_window,
            clock,
            shutdown: AtomicBool::new(false),
        });
        let poller = spawn_poller(Arc::clone(&inner), events_rx, poll_interval);
        Self {
            inner,
            poller: Some(poller),
        }
    }

    /// Add a rule and, if it's enabled, start watching its folder. A folder that can't
    /// be watched (missing, permissions) doesn't fail the call — the rule is added with
    /// an `error` status (04 §S5 "watch folder unreachable" badge) and starts working as
    /// soon as [`WatchService::retry_unreachable`] or a re-enable succeeds.
    pub fn add_rule(&self, mut rule: WatchRule) -> WatchRuleId {
        // Canonicalize the watch dir and output dir up front so every path this rule
        // later derives — job outputs, the processed-log sidecar location, the paths fed
        // to the stability tracker — shares the single canonical form
        // `consider_candidate` compares incoming events against (see `normalize`).
        rule.folder = normalize(&rule.folder);
        rule.output_dir = normalize(&rule.output_dir);
        let id = rule.id;
        let tracker =
            StabilityTracker::new(self.inner.stability_window, Arc::clone(&self.inner.clock));
        let processed = ProcessedLog::load(&rule.output_dir);
        let mut runtime = RuleRuntime {
            rule,
            watcher: None,
            tracker,
            processed,
            known_outputs: HashSet::new(),
            error: None,
        };
        if runtime.rule.enabled {
            self.start_watching(&mut runtime);
        }
        self.inner.state.lock().rules.insert(id, runtime);
        id
    }

    /// Stop and forget a rule entirely. Returns `false` if `id` is unknown.
    pub fn remove_rule(&self, id: WatchRuleId) -> bool {
        self.inner.state.lock().rules.remove(&id).is_some()
    }

    /// Turn a rule on (starts its watcher) or off (stops watching; anything mid-
    /// stability-check for that rule is dropped, not queued). Returns `false` if `id` is
    /// unknown.
    pub fn set_enabled(&self, id: WatchRuleId, enabled: bool) -> bool {
        let mut state = self.inner.state.lock();
        let Some(runtime) = state.rules.get_mut(&id) else {
            return false;
        };
        runtime.rule.enabled = enabled;
        if enabled {
            self.start_watching(runtime);
        } else {
            runtime.watcher = None;
        }
        true
    }

    /// Retry starting the watcher for every rule currently showing an error (e.g. a
    /// network share that just came back online).
    pub fn retry_unreachable(&self) {
        let mut state = self.inner.state.lock();
        for runtime in state.rules.values_mut() {
            if runtime.rule.enabled && runtime.watcher.is_none() {
                self.start_watching(runtime);
            }
        }
    }

    /// Every rule's current status, for the S5 rule list.
    pub fn list_rules(&self) -> Vec<WatchRuleStatus> {
        self.inner
            .state
            .lock()
            .rules
            .values()
            .map(|r| WatchRuleStatus {
                rule: r.rule.clone(),
                error: r.error.clone(),
            })
            .collect()
    }

    fn start_watching(&self, runtime: &mut RuleRuntime) {
        match build_watcher(
            runtime.rule.id,
            &runtime.rule.folder,
            self.inner.events_tx.clone(),
        ) {
            Ok(watcher) => {
                runtime.watcher = Some(watcher);
                runtime.error = None;
            }
            Err(e) => {
                runtime.watcher = None;
                runtime.error = Some(e.to_string());
            }
        }
    }
}

impl Drop for WatchService {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.poller.take() {
            let _ = handle.join();
        }
    }
}

fn build_watcher(
    id: WatchRuleId,
    folder: &Path,
    tx: crossbeam_channel::Sender<(WatchRuleId, PathBuf)>,
) -> Result<RecommendedWatcher, BatchError> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if event.kind.is_create() || event.kind.is_modify() {
                for path in event.paths {
                    let _ = tx.send((id, path));
                }
            }
        }
    })?;
    watcher.watch(folder, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

fn spawn_poller(
    inner: Arc<Inner>,
    events_rx: crossbeam_channel::Receiver<(WatchRuleId, PathBuf)>,
    poll_interval: Duration,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("anvil-watch-poll".into())
        .spawn(move || poll_loop(&inner, &events_rx, poll_interval))
        .expect("failed to spawn watch poller thread")
}

/// One tick: drain fresh `notify` events into their rule's tracker as candidates, then
/// re-check every tracked candidate's current size against the stability window and
/// submit whatever just became stable. Runs on its own thread so a slow or wedged queue
/// never blocks event delivery.
fn poll_loop(
    inner: &Arc<Inner>,
    events_rx: &crossbeam_channel::Receiver<(WatchRuleId, PathBuf)>,
    poll_interval: Duration,
) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }

        while let Ok((id, path)) = events_rx.try_recv() {
            let mut state = inner.state.lock();
            if let Some(runtime) = state.rules.get_mut(&id) {
                consider_candidate(runtime, &path);
            }
        }

        let ready = collect_stable_jobs(inner);
        for (id, job, fingerprint) in ready {
            inner.queue.submit_jobs(vec![job.clone()]);
            let mut state = inner.state.lock();
            if let Some(runtime) = state.rules.get_mut(&id) {
                runtime.known_outputs.insert(normalize(&job.output));
                runtime.processed.mark_processed(&job.input, fingerprint);
                runtime.processed.save(&runtime.rule.output_dir);
            }
        }

        sleep_respecting_shutdown(&inner.shutdown, poll_interval);
    }
}

/// Canonicalize `path` into the single form used everywhere the watch engine compares
/// paths or uses one as a map/set key.
///
/// macOS is the reason this exists: `/var`, `/tmp`, and many user paths are symlinks into
/// `/private`, and the `notify` FSEvents backend reports the resolved `/private/...` form
/// — while the paths we build from a rule's user-supplied `output_dir` keep the symlinked
/// form. Comparing the two directly makes the "never re-process our own output" guard
/// silently miss and re-queue a rendered file as a fresh input. Routing every path
/// through here collapses both forms to one.
///
/// `std::fs::canonicalize` requires the path to exist, but an output path is registered
/// before its render has created the file, so fall back to canonicalizing the (already
/// existing) parent directory and re-joining the file name — the file component is never
/// itself a symlink, so this matches what canonicalizing the whole path would yield once
/// the file lands. If even that fails (the file vanished mid-flight, or there's no
/// parent), keep the original path. On Windows, `fs::canonicalize` yields `\\?\`-verbatim
/// paths; [`strip_verbatim`] folds those back to the conventional form uniformly, so
/// comparisons stay consistent (both sides of every one run through this same helper)
/// and the rule paths a UI displays stay presentable.
fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_verbatim(canonical);
    }
    // Walk up to the nearest EXISTING ancestor, canonicalize that, and re-append the
    // not-yet-created tail. One level is not enough: an output path can be registered
    // whole directories before its render creates them (a fresh output dir nested in
    // the watch dir), and a key built from the raw form would miss the canonical form
    // the later filesystem event arrives in.
    let mut tail = Vec::new();
    let mut cursor = path;
    while let (Some(parent), Some(name)) = (cursor.parent(), cursor.file_name()) {
        tail.push(name);
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            let mut out = canonical_parent;
            for name in tail.into_iter().rev() {
                out.push(name);
            }
            return strip_verbatim(out);
        }
        cursor = parent;
    }
    strip_verbatim(path.to_path_buf())
}

/// Strip Windows' `\\?\` verbatim prefix back to the conventional form, dunce-style
/// (`\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` → `\\server\share\x`): rule paths end
/// up displayed verbatim in a UI's watch-rule list, so the canonical form kept for
/// comparisons must also be the presentable one. The prefix is preserved when the
/// stripped form would exceed the classic 259-unit `MAX_PATH` budget (the one case where
/// verbatim is load-bearing) or isn't a plain drive/UNC path (e.g. `\\?\Volume{..}\`,
/// which has no conventional spelling). Pure string manipulation, no filesystem calls —
/// canonical Unix paths start with `/` and pass through untouched, and the logic is
/// unit-tested on every platform.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    // Longest path Windows accepts without the verbatim prefix (MAX_PATH minus the
    // terminating NUL), measured in UTF-16 units like the OS does.
    const MAX_PATH_WITHOUT_VERBATIM: usize = 259;

    let Some(s) = path.to_str() else {
        return path;
    };
    let stripped = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Only the drive-letter form (`C:\..`) is safe to strip; other verbatim roots
        // stay as they are.
        let b = rest.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
            rest.to_owned()
        } else {
            return path;
        }
    } else {
        return path;
    };
    if stripped.encode_utf16().count() > MAX_PATH_WITHOUT_VERBATIM {
        return path;
    }
    PathBuf::from(stripped)
}

fn consider_candidate(runtime: &mut RuleRuntime, path: &Path) {
    // Normalize the reported path once, here at ingress, so every key derived from it
    // downstream (`known_outputs` lookups, the stability tracker, the processed log) is
    // in the same canonical form the rest of the engine stores — see `normalize`.
    let path = normalize(path);
    let path = path.as_path();

    if processed_log::is_log_file(path) {
        return;
    }
    if runtime.known_outputs.contains(path) {
        return; // our own output — never a candidate, however it was reported
    }
    if !path.is_file() {
        return;
    }
    if !runtime.rule.pattern.matches(path) {
        return;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        runtime.tracker.observe(path, meta.len());
    }
}

/// Re-check reachability and every tracked candidate for each enabled rule, returning
/// the jobs that just crossed the stability threshold (and aren't already-processed).
/// Building the list without holding the lock across the submit avoids holding
/// `state` while calling into the queue.
fn collect_stable_jobs(inner: &Arc<Inner>) -> Vec<(WatchRuleId, BatchJob, (u64, u64))> {
    let mut ready = Vec::new();
    let mut state = inner.state.lock();
    for (id, runtime) in state.rules.iter_mut() {
        if !runtime.rule.enabled {
            continue;
        }
        if std::fs::metadata(&runtime.rule.folder).is_err() {
            runtime.error = Some("watch folder unreachable".into());
            continue;
        }
        if runtime.error.as_deref() == Some("watch folder unreachable") {
            runtime.error = None;
        }

        for path in runtime.tracker.tracked_paths() {
            let Ok(meta) = std::fs::metadata(&path) else {
                runtime.tracker.forget(&path); // disappeared mid-copy or renamed away
                continue;
            };
            let size = meta.len();
            if !runtime.tracker.observe(&path, size) {
                continue;
            }
            runtime.tracker.forget(&path);

            let fingerprint = (size, mtime_ms(&meta));
            if runtime.processed.already_processed(&path, fingerprint) {
                continue;
            }

            let output = resolve_output(
                &path,
                &runtime.rule.folder,
                &OutputSettings::new(runtime.rule.output_dir.clone()),
            );
            ready.push((
                *id,
                BatchJob {
                    input: path,
                    output,
                    preset: runtime.rule.preset.clone(),
                    tier: runtime.rule.tier,
                },
                fingerprint,
            ));
        }
    }
    ready
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sleep_respecting_shutdown(shutdown: &AtomicBool, total: Duration) {
    let step = Duration::from_millis(10)
        .min(total)
        .max(Duration::from_millis(1));
    let mut waited = Duration::ZERO;
    while waited < total {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(step);
        waited += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_project::{Preset, Tier};

    fn write_wav_fixture(path: &Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..4_800u32 {
            let s = (0.2 * (i as f32 * 0.05).sin() * i16::MAX as f32) as i16;
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn growing_file_is_not_queued_until_stable_then_queued_once() {
        let tmp = tempfile::tempdir().unwrap();
        let watch_dir = tmp.path().join("in");
        let out_dir = tmp.path().join("out");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let queue = Arc::new(BatchQueue::with_concurrency(1).unwrap());
        let service = WatchService::with_config(
            Arc::clone(&queue),
            Duration::from_millis(100),
            Duration::from_millis(20),
            Arc::new(SystemClock),
        );
        service.add_rule(WatchRule::new(
            &watch_dir,
            Preset::default(),
            Tier::Fast,
            &out_dir,
        ));

        let target = watch_dir.join("ep1.wav");
        // Simulate a copy-in-progress: two growing writes before the real content lands.
        std::fs::write(&target, vec![0u8; 500]).unwrap();
        thread::sleep(Duration::from_millis(40));
        std::fs::write(&target, vec![0u8; 900]).unwrap();

        thread::sleep(Duration::from_millis(60));
        assert!(
            queue.snapshot().is_empty(),
            "a file still changing size must not be queued"
        );

        // Let it hold steady past the stability window.
        thread::sleep(Duration::from_millis(250));
        assert_eq!(
            queue.snapshot().len(),
            1,
            "a file that stopped changing should be queued exactly once"
        );
    }

    #[test]
    fn rendered_output_is_never_picked_up_as_a_new_input() {
        let tmp = tempfile::tempdir().unwrap();
        let watch_dir = tmp.path().join("in");
        std::fs::create_dir_all(&watch_dir).unwrap();
        // Output lands directly in the watched folder (the risky configuration) —
        // exercises the "never re-process outputs" guard for real.
        let out_dir = watch_dir.clone();

        let queue = Arc::new(BatchQueue::with_concurrency(1).unwrap());
        let service = WatchService::with_config(
            Arc::clone(&queue),
            Duration::from_millis(60),
            Duration::from_millis(15),
            Arc::new(SystemClock),
        );
        service.add_rule(WatchRule::new(
            &watch_dir,
            Preset::default(),
            Tier::Fast,
            &out_dir,
        ));

        write_wav_fixture(&watch_dir.join("ep1.wav"));

        // Wait for it to stabilize, get queued, and actually render (writing
        // "ep1_mastered.wav" straight into the watched folder).
        thread::sleep(Duration::from_millis(150));
        assert!(queue.wait_idle(Duration::from_secs(15)));
        assert_eq!(
            queue.snapshot().len(),
            1,
            "only the original input should ever be queued"
        );

        // Give the watcher plenty of chances to notice the new output file and
        // (incorrectly, if the guard failed) queue it too.
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            queue.snapshot().len(),
            1,
            "the file this service just rendered must never be queued as a new input"
        );
    }

    #[test]
    fn disabled_rule_does_not_queue_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        let watch_dir = tmp.path().join("in");
        let out_dir = tmp.path().join("out");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let queue = Arc::new(BatchQueue::with_concurrency(1).unwrap());
        let service = WatchService::with_config(
            Arc::clone(&queue),
            Duration::from_millis(40),
            Duration::from_millis(10),
            Arc::new(SystemClock),
        );
        let mut rule = WatchRule::new(&watch_dir, Preset::default(), Tier::Fast, &out_dir);
        rule.enabled = false;
        service.add_rule(rule);

        std::fs::write(watch_dir.join("ep1.wav"), vec![0u8; 500]).unwrap();
        thread::sleep(Duration::from_millis(150));
        assert!(queue.snapshot().is_empty());
    }

    #[test]
    fn unreachable_folder_is_reported_as_a_rule_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let out_dir = tmp.path().join("out");

        let queue = Arc::new(BatchQueue::with_concurrency(1).unwrap());
        let service = WatchService::new(Arc::clone(&queue));
        service.add_rule(WatchRule::new(
            &missing,
            Preset::default(),
            Tier::Fast,
            &out_dir,
        ));

        let statuses = service.list_rules();
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].error.is_some());
    }

    #[test]
    fn normalize_resolves_through_not_yet_created_directories() {
        let tmp = tempfile::tempdir().unwrap();
        // Two directory levels that do NOT exist yet — like an output dir the first
        // render will create. normalize must still resolve the existing ancestor's
        // symlinks (macOS: /var → /private/var) so the key matches the canonical form
        // the eventual filesystem event reports.
        let deep = tmp
            .path()
            .join("out_new")
            .join("nested")
            .join("ep1_mastered.wav");
        let expected = strip_verbatim(std::fs::canonicalize(tmp.path()).unwrap())
            .join("out_new")
            .join("nested")
            .join("ep1_mastered.wav");
        assert_eq!(normalize(&deep), expected);
    }

    // `strip_verbatim` is pure string manipulation, so its Windows-shaped inputs are
    // exercised on every platform with hardcoded verbatim forms.

    #[test]
    fn strip_verbatim_folds_drive_letter_form_to_conventional() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\Users\nb\out\ep1_mastered.wav")),
            PathBuf::from(r"C:\Users\nb\out\ep1_mastered.wav")
        );
    }

    #[test]
    fn strip_verbatim_folds_unc_form_to_conventional() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\nas\share\out\ep1_mastered.wav")),
            PathBuf::from(r"\\nas\share\out\ep1_mastered.wav")
        );
    }

    #[test]
    fn strip_verbatim_keeps_prefix_when_stripped_form_exceeds_max_path() {
        let long = format!(r"\\?\C:\{}\ep1.wav", "a".repeat(300));
        assert_eq!(strip_verbatim(PathBuf::from(&long)), PathBuf::from(&long));
    }

    #[test]
    fn strip_verbatim_leaves_unstrippable_paths_alone() {
        // Canonical Unix form — what macOS/Linux canonicalization produces.
        assert_eq!(
            strip_verbatim(PathBuf::from("/private/var/x/ep1.wav")),
            PathBuf::from("/private/var/x/ep1.wav")
        );
        // Already-conventional Windows forms.
        assert_eq!(
            strip_verbatim(PathBuf::from(r"C:\x\ep1.wav")),
            PathBuf::from(r"C:\x\ep1.wav")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\nas\share\ep1.wav")),
            PathBuf::from(r"\\nas\share\ep1.wav")
        );
        // A verbatim root with no conventional spelling stays verbatim.
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\Volume{b75e2c83-0000}\ep1.wav")),
            PathBuf::from(r"\\?\Volume{b75e2c83-0000}\ep1.wav")
        );
    }
}
