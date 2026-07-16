//! # anvil-batch
//!
//! Batch queue and watch-folder engine (04 §S4/S5, M2). Runs many files through
//! `anvil_dsp::master` on the shared job system with per-file isolation (one failure never
//! sinks the batch), concurrency scaled to N-1 cores, and progress events; plus watch rules
//! that stably detect new files (size-stable check) and never re-process their own outputs.
//!
//! UI-independent: the desktop Batch (S4) / Watch (S5) screens and the `anvil batch` CLI are
//! thin drivers over this. The M2 batch lane fills in `queue` and `watch` modules here.
//!
//! ## Layout
//! - [`queue`] — [`BatchQueue`]: the concurrency-controlled, per-file-isolated engine
//!   that runs [`BatchJob`]s against `anvil_dsp::master` and exposes their status.
//! - [`catalog`] — flat-file and back-catalog (recursive folder, optional structure
//!   preservation) helpers that build [`BatchJob`]s for [`BatchQueue`].
//! - [`watch`] — [`WatchService`]: `notify`-backed watch rules with a size-stable check
//!   and duplicate-output protection, feeding the same [`BatchQueue`].
//! - [`error`] — [`BatchError`], this crate's error surface.

pub mod catalog;
pub mod error;
pub mod queue;
pub mod watch;

pub use catalog::OutputSettings;
pub use error::BatchError;
pub use queue::{BatchItemState, BatchItemStatus, BatchJob, BatchJobId, BatchQueue};
pub use watch::{FilePattern, WatchRule, WatchRuleId, WatchRuleStatus, WatchService};
