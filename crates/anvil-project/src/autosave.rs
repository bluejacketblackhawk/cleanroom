//! Debounced/coalescing autosave policy (ADR-008: "Autosave every 30 s + on close").
//!
//! [`Autosave`] only decides *when* a save is due — it owns no timer thread and performs
//! no I/O itself, so it needs no real sleeps to test: callers (a UI event loop, or a
//! test) drive it with explicit [`std::time::Instant`]s. A typical integration calls
//! [`Autosave::notify_dirty`] on every edit and [`Autosave::maybe_save`] on a periodic
//! tick (or before closing the project).

use std::path::Path;
use std::time::{Duration, Instant};

use anvil_core::Result;

use crate::Project;

/// Coalesces rapid edits into a single save once the project has been dirty for at least
/// `debounce`. The timer starts at the *first* edit in a burst and does **not** reset on
/// subsequent edits — a project edited continuously still autosaves every `debounce`
/// interval rather than the save being pushed out indefinitely (matches the fixed 30 s
/// cadence in ADR-008, as opposed to a classic UI debounce that restarts on every
/// keystroke).
pub struct Autosave {
    debounce: Duration,
    dirty_since: Option<Instant>,
    last_saved: Option<Instant>,
}

impl Autosave {
    /// A new autosave policy that considers a save due `debounce` after the first dirty
    /// edit in a burst.
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            dirty_since: None,
            last_saved: None,
        }
    }

    /// Mark the project dirty as of `now`. No-op if already dirty (the clock doesn't
    /// restart).
    pub fn notify_dirty(&mut self, now: Instant) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(now);
        }
    }

    /// Whether a save is due at `now`: dirty, and at least `debounce` has elapsed since
    /// the burst started.
    pub fn is_due(&self, now: Instant) -> bool {
        match self.dirty_since {
            Some(since) => now.saturating_duration_since(since) >= self.debounce,
            None => false,
        }
    }

    /// Record that a save happened at `now`: clears the dirty flag so [`Self::is_due`]
    /// returns `false` until the next [`Self::notify_dirty`].
    pub fn mark_saved(&mut self, now: Instant) {
        self.dirty_since = None;
        self.last_saved = Some(now);
    }

    /// When the last save happened, if any.
    pub fn last_saved(&self) -> Option<Instant> {
        self.last_saved
    }

    /// Whether the project currently has unsaved changes pending (dirty, regardless of
    /// whether the debounce has elapsed yet).
    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// If a save is due at `now`, save `project` to `dir` and mark saved. Returns
    /// whether it actually saved.
    pub fn maybe_save(&mut self, now: Instant, project: &Project, dir: &Path) -> Result<bool> {
        if !self.is_due(now) {
            return Ok(false);
        }
        project.save(dir)?;
        self.mark_saved(now);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_due_before_debounce_elapses() {
        let mut autosave = Autosave::new(Duration::from_secs(30));
        let t0 = Instant::now();
        autosave.notify_dirty(t0);
        assert!(!autosave.is_due(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn due_once_debounce_elapses() {
        let mut autosave = Autosave::new(Duration::from_secs(30));
        let t0 = Instant::now();
        autosave.notify_dirty(t0);
        assert!(autosave.is_due(t0 + Duration::from_secs(30)));
        assert!(autosave.is_due(t0 + Duration::from_secs(45)));
    }

    #[test]
    fn repeated_dirty_calls_coalesce_to_the_first_edit() {
        let mut autosave = Autosave::new(Duration::from_secs(30));
        let t0 = Instant::now();
        autosave.notify_dirty(t0);
        // A burst of further edits shortly after t0 must not push the due time out.
        autosave.notify_dirty(t0 + Duration::from_secs(20));
        autosave.notify_dirty(t0 + Duration::from_secs(29));
        assert!(!autosave.is_due(t0 + Duration::from_secs(29)));
        assert!(autosave.is_due(t0 + Duration::from_secs(30)));
    }

    #[test]
    fn mark_saved_clears_dirty_state() {
        let mut autosave = Autosave::new(Duration::from_secs(30));
        let t0 = Instant::now();
        autosave.notify_dirty(t0);
        autosave.mark_saved(t0 + Duration::from_secs(30));
        assert!(!autosave.is_dirty());
        assert!(!autosave.is_due(t0 + Duration::from_secs(100)));
        assert_eq!(autosave.last_saved(), Some(t0 + Duration::from_secs(30)));
    }

    #[test]
    fn maybe_save_writes_to_disk_only_when_due() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("show.anvilproj");
        let project = Project::new("show");
        let mut autosave = Autosave::new(Duration::from_secs(30));

        let t0 = Instant::now();
        autosave.notify_dirty(t0);

        assert!(!autosave
            .maybe_save(t0 + Duration::from_secs(5), &project, &dir)
            .unwrap());
        assert!(!dir.join(crate::PROJECT_MANIFEST_FILE).exists());

        assert!(autosave
            .maybe_save(t0 + Duration::from_secs(30), &project, &dir)
            .unwrap());
        assert!(dir.join(crate::PROJECT_MANIFEST_FILE).exists());
        assert!(!autosave.is_dirty());
    }
}
