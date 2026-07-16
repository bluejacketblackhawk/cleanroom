//! The size-stable check (04 §S5): a new file isn't queued until its size has held
//! steady for a window (default 5 s) — handles a copy-in-progress and rename storms
//! (create + a burst of renames/modifies as an editor or copier finishes writing).
//!
//! Time is injected via [`Clock`] rather than read from the OS directly, so this logic
//! is fully unit-testable without real sleeps: a test clock can jump forward instantly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Abstracts "now" as milliseconds since an arbitrary fixed epoch. Only relative
/// differences are used, so any monotonically-nondecreasing source works.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The real wall clock, used by [`super::WatchService`] in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingFile {
    size: u64,
    last_changed_ms: u64,
}

/// Tracks candidate files and reports once each has held a steady size for the
/// configured window. One tracker per watch rule.
pub struct StabilityTracker {
    window_ms: u64,
    clock: Arc<dyn Clock>,
    pending: HashMap<PathBuf, PendingFile>,
}

impl StabilityTracker {
    pub fn new(window: Duration, clock: Arc<dyn Clock>) -> Self {
        Self {
            window_ms: window.as_millis() as u64,
            clock,
            pending: HashMap::new(),
        }
    }

    /// Record an observed size for `path` "now". Returns `true` once the file has held
    /// that exact size for at least the stability window — i.e. it's safe to process.
    /// A size change (the file is still growing, or was truncated/rewritten) resets the
    /// window.
    pub fn observe(&mut self, path: &Path, size: u64) -> bool {
        let now = self.clock.now_ms();
        match self.pending.get_mut(path) {
            Some(entry) if entry.size == size => {
                now.saturating_sub(entry.last_changed_ms) >= self.window_ms
            }
            Some(entry) => {
                entry.size = size;
                entry.last_changed_ms = now;
                false
            }
            None => {
                self.pending.insert(
                    path.to_path_buf(),
                    PendingFile {
                        size,
                        last_changed_ms: now,
                    },
                );
                false
            }
        }
    }

    /// Stop tracking `path` — it was processed, disappeared, or was explicitly dropped
    /// from the candidate set (e.g. it stopped matching the rule's pattern).
    pub fn forget(&mut self, path: &Path) {
        self.pending.remove(path);
    }

    pub fn is_tracking(&self, path: &Path) -> bool {
        self.pending.contains_key(path)
    }

    /// Paths currently being tracked (for diagnostics / tests).
    pub fn tracked_paths(&self) -> Vec<PathBuf> {
        self.pending.keys().cloned().collect()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Clock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A clock a test fully controls: starts at 0, advances only when told to. Lets
    /// stability-window tests exercise "5 seconds pass" without ever sleeping.
    #[derive(Default)]
    pub struct TestClock(AtomicU64);

    impl TestClock {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestClock;
    use super::*;

    #[test]
    fn first_observation_is_never_immediately_stable() {
        let clock = TestClock::new();
        let mut tracker = StabilityTracker::new(Duration::from_secs(5), clock);
        assert!(!tracker.observe(Path::new("f.wav"), 100));
    }

    #[test]
    fn becomes_stable_once_the_window_elapses_at_the_same_size() {
        let clock = TestClock::new();
        let mut tracker =
            StabilityTracker::new(Duration::from_secs(5), Arc::clone(&clock) as Arc<dyn Clock>);

        assert!(!tracker.observe(Path::new("f.wav"), 100));
        clock.advance(4_999);
        assert!(
            !tracker.observe(Path::new("f.wav"), 100),
            "not stable yet at 4.999s"
        );
        clock.advance(1);
        assert!(
            tracker.observe(Path::new("f.wav"), 100),
            "stable at exactly 5s"
        );
    }

    #[test]
    fn a_growing_file_never_reports_stable() {
        let clock = TestClock::new();
        let mut tracker =
            StabilityTracker::new(Duration::from_secs(5), Arc::clone(&clock) as Arc<dyn Clock>);

        assert!(!tracker.observe(Path::new("f.wav"), 100));
        clock.advance(4_000);
        // Still growing at 4s in -> resets the window.
        assert!(!tracker.observe(Path::new("f.wav"), 200));
        clock.advance(4_999);
        assert!(
            !tracker.observe(Path::new("f.wav"), 200),
            "only 4.999s since the last change"
        );
        clock.advance(1);
        assert!(
            tracker.observe(Path::new("f.wav"), 200),
            "5s since size last changed"
        );
    }

    #[test]
    fn rename_storm_of_repeated_same_size_events_still_settles() {
        // Simulates a burst of Modify events for the same final size (a common rename-
        // storm pattern) — each observation at unchanged size should just keep counting
        // toward the same window, not restart it.
        let clock = TestClock::new();
        let mut tracker =
            StabilityTracker::new(Duration::from_secs(5), Arc::clone(&clock) as Arc<dyn Clock>);

        assert!(!tracker.observe(Path::new("f.wav"), 100));
        for _ in 0..10 {
            clock.advance(400);
            tracker.observe(Path::new("f.wav"), 100);
        }
        // 10 * 400ms = 4000ms elapsed so far.
        clock.advance(1_000);
        assert!(tracker.observe(Path::new("f.wav"), 100));
    }

    #[test]
    fn forget_drops_tracking_state() {
        let clock = TestClock::new();
        let mut tracker =
            StabilityTracker::new(Duration::from_secs(5), Arc::clone(&clock) as Arc<dyn Clock>);
        tracker.observe(Path::new("f.wav"), 100);
        assert!(tracker.is_tracking(Path::new("f.wav")));
        tracker.forget(Path::new("f.wav"));
        assert!(!tracker.is_tracking(Path::new("f.wav")));
    }
}
