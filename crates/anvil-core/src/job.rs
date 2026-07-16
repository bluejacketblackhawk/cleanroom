//! Job system skeleton: identifiers, cancellation tokens, and progress events.
//!
//! Heavy work runs on Rust worker threads; Tauri commands return a [`JobId`] immediately
//! and stream [`Progress`] back to the UI (ADR: process model, 02 §Non-obvious
//! consequences). The full thread-pool/scheduler lands in M0 lane E — this module fixes
//! the vocabulary the rest of the engine builds on.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Opaque identifier for a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub Uuid);

impl JobId {
    /// Mint a fresh random job id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

/// Lifecycle state of a job, mirrored to the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Cancelled,
    Failed,
}

/// A progress update emitted over the `job://progress` channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    /// Completion in `[0.0, 1.0]`.
    pub fraction: f32,
    /// Human-readable stage label (e.g. "Analyzing", "Denoising").
    pub message: String,
}

/// A cheap, cloneable cooperative-cancellation flag shared between the UI-facing handle
/// and the worker. Workers call [`CancellationToken::check`] at block boundaries.
#[derive(Clone, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Returns [`crate::Error::Cancelled`] if cancellation was requested.
    pub fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(crate::Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Reports [`Progress`] from inside a running job. Cheap to clone; a job typically
/// clones this into any sub-stage that needs to emit its own updates.
#[derive(Clone)]
pub struct ProgressReporter {
    tx: Sender<Progress>,
}

impl ProgressReporter {
    /// Emit a progress update. Best-effort: if nobody holds the receiving end anymore
    /// (the [`JobHandle`] was dropped), the update is silently discarded rather than
    /// erroring the job.
    pub fn report(&self, fraction: f32, message: impl Into<String>) {
        let _ = self.tx.send(Progress {
            fraction,
            message: message.into(),
        });
    }
}

/// State shared between a [`JobHandle`] and the worker thread executing its job.
/// `state` and `cond` implement the blocking half of [`JobHandle::join`]; `outcome`
/// carries the job's return value across once, the same way a [`std::thread::JoinHandle`]
/// hands back its result.
struct Shared<T> {
    state: Mutex<JobState>,
    cond: Condvar,
    outcome: Mutex<Option<crate::Result<T>>>,
}

/// A handle to a job submitted to a [`JobScheduler`].
///
/// Cheap to hold onto: querying [`JobHandle::state`] or [`JobHandle::progress`] never
/// blocks. Retrieving the job's return value ([`JobHandle::join`] /
/// [`JobHandle::try_result`]) hands it back exactly once — call one of them a single
/// time per job, the same contract as [`std::thread::JoinHandle`].
pub struct JobHandle<T> {
    id: JobId,
    token: CancellationToken,
    progress_rx: Receiver<Progress>,
    shared: Arc<Shared<T>>,
}

impl<T> JobHandle<T> {
    /// The id minted for this job at submission time.
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Current lifecycle state. Safe to poll repeatedly; never blocks.
    pub fn state(&self) -> JobState {
        *self.shared.state.lock()
    }

    /// Request cooperative cancellation. Idempotent; has no effect once the job has
    /// already reached a terminal state. The job itself decides when it's safe to stop
    /// by calling [`CancellationToken::check`] (or `is_cancelled`) at its own block
    /// boundaries — this only raises the flag.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Subscribe to this job's progress stream. Each call returns a clone of the same
    /// underlying `crossbeam_channel` receiver: `crossbeam_channel` is MPMC, so if
    /// multiple subscribers call `recv()` concurrently, updates are load-balanced across
    /// them (each message goes to exactly one receiver), not broadcast to every
    /// subscriber. Fine for the common case of one UI listener per job.
    pub fn progress(&self) -> Receiver<Progress> {
        self.progress_rx.clone()
    }

    /// Block the calling thread until the job reaches a terminal state, then return its
    /// result. Calling this more than once (or mixed with [`Self::try_result`]) after
    /// the value has already been retrieved returns [`crate::Error::Other`] rather than
    /// blocking forever, since the value can't be handed back twice.
    pub fn join(&self) -> crate::Result<T> {
        let mut state = self.shared.state.lock();
        while !is_terminal(*state) {
            self.shared.cond.wait(&mut state);
        }
        drop(state);
        self.take_outcome()
    }

    /// Non-blocking poll. Returns `None` while the job is still queued or running, or
    /// once its result has already been retrieved; returns `Some(result)` exactly once,
    /// as soon as the job finishes.
    pub fn try_result(&self) -> Option<crate::Result<T>> {
        if !is_terminal(self.state()) {
            return None;
        }
        self.shared.outcome.lock().take()
    }

    fn take_outcome(&self) -> crate::Result<T> {
        self.shared
            .outcome
            .lock()
            .take()
            .unwrap_or_else(|| Err(crate::Error::Other("job result already retrieved".into())))
    }
}

fn is_terminal(state: JobState) -> bool {
    matches!(
        state,
        JobState::Done | JobState::Cancelled | JobState::Failed
    )
}

/// A thread-pool job scheduler: heavy work is submitted here and runs off the calling
/// thread, which gets a [`JobHandle`] back immediately (ADR-010 process model — the UI
/// never blocks on a render/analysis pass).
///
/// Backed by a private `rayon` thread pool (not the process-global one, so tests and
/// multiple schedulers stay isolated from each other). Thread-safe: `submit` takes `&self`
/// and may be called concurrently from multiple threads; a `JobScheduler` is typically
/// held in an `Arc` and shared across the app.
///
/// Cancellation is cooperative: jobs receive a [`CancellationToken`] and are expected to
/// call [`CancellationToken::check`] at their own block/iteration boundaries and
/// propagate the resulting [`crate::Error::Cancelled`]. A job that never checks the token
/// runs to completion even after [`JobHandle::cancel`] is called.
pub struct JobScheduler {
    pool: rayon::ThreadPool,
}

impl JobScheduler {
    /// Build a scheduler with `rayon`'s default thread count (the number of logical
    /// CPUs).
    pub fn new() -> crate::Result<Self> {
        Self::with_threads(0)
    }

    /// Build a scheduler with a specific worker-thread count. `0` defers to `rayon`'s
    /// default (logical CPU count).
    pub fn with_threads(num_threads: usize) -> crate::Result<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("anvil-job-{i}"))
            .build()
            .map_err(|e| crate::Error::Other(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Submit a job and return immediately with a handle to it. `job` receives a
    /// [`CancellationToken`] to poll and a [`ProgressReporter`] to emit updates through,
    /// and returns the job's result.
    ///
    /// A job that panics is caught: the scheduler itself is never poisoned by it, the
    /// panic message is captured into [`crate::Error::JobPanicked`], and the job's state
    /// settles to [`JobState::Failed`] like any other error — subsequent `submit` calls
    /// on the same scheduler are unaffected.
    pub fn submit<T, F>(&self, job: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(&CancellationToken, &ProgressReporter) -> crate::Result<T> + Send + 'static,
    {
        let id = JobId::new();
        let token = CancellationToken::new();
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            state: Mutex::new(JobState::Queued),
            cond: Condvar::new(),
            outcome: Mutex::new(None),
        });

        let worker_token = token.clone();
        let worker_shared = Arc::clone(&shared);
        let reporter = ProgressReporter { tx: progress_tx };

        self.pool.spawn(move || {
            *worker_shared.state.lock() = JobState::Running;

            let result =
                match panic::catch_unwind(AssertUnwindSafe(|| job(&worker_token, &reporter))) {
                    Ok(result) => result,
                    Err(payload) => Err(crate::Error::JobPanicked(panic_message(&payload))),
                };

            let final_state = match &result {
                Ok(_) => JobState::Done,
                Err(crate::Error::Cancelled) => JobState::Cancelled,
                Err(_) => JobState::Failed,
            };

            *worker_shared.outcome.lock() = Some(result);
            let mut state = worker_shared.state.lock();
            *state = final_state;
            worker_shared.cond.notify_all();
        });

        JobHandle {
            id,
            token,
            progress_rx,
            shared,
        }
    }
}

/// Best-effort extraction of a human-readable message from a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "job panicked with a non-string payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_reports_cancellation() {
        let token = CancellationToken::new();
        assert!(token.check().is_ok());
        token.cancel();
        assert!(token.is_cancelled());
        assert!(matches!(token.check(), Err(crate::Error::Cancelled)));
    }

    #[test]
    fn job_ids_are_unique() {
        assert_ne!(JobId::new(), JobId::new());
    }

    #[test]
    fn scheduler_runs_a_job_to_completion() {
        let scheduler = JobScheduler::with_threads(2).unwrap();
        let handle = scheduler.submit(|_token, _progress| Ok::<_, crate::Error>(21 * 2));
        assert_eq!(handle.join().unwrap(), 42);
        assert_eq!(handle.state(), JobState::Done);
    }

    #[test]
    fn scheduler_cancels_a_long_job() {
        let scheduler = JobScheduler::with_threads(2).unwrap();
        let handle = scheduler.submit(|token, _progress| {
            loop {
                token.check()?;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            #[allow(unreachable_code)]
            Ok::<(), crate::Error>(())
        });

        // Give the worker a moment to actually start looping before cancelling.
        std::thread::sleep(std::time::Duration::from_millis(20));
        handle.cancel();

        assert!(matches!(handle.join(), Err(crate::Error::Cancelled)));
        assert_eq!(handle.state(), JobState::Cancelled);
    }

    #[test]
    fn scheduler_delivers_progress_in_order() {
        let scheduler = JobScheduler::with_threads(2).unwrap();
        let handle = scheduler.submit(|_token, progress| {
            for i in 0..5 {
                progress.report(i as f32 / 4.0, format!("step {i}"));
            }
            Ok::<_, crate::Error>(())
        });

        let rx = handle.progress();
        handle.join().unwrap();

        let received: Vec<Progress> = rx.try_iter().collect();
        assert_eq!(received.len(), 5);
        for (i, update) in received.iter().enumerate() {
            assert_eq!(update.fraction, i as f32 / 4.0);
            assert_eq!(update.message, format!("step {i}"));
        }
    }

    #[test]
    fn scheduler_marks_panicking_job_failed_without_poisoning() {
        let scheduler = JobScheduler::with_threads(2).unwrap();

        let panicking = scheduler.submit(|_token, _progress| -> crate::Result<()> {
            panic!("boom");
        });
        let err = panicking.join().unwrap_err();
        assert!(matches!(err, crate::Error::JobPanicked(_)));
        assert_eq!(panicking.state(), JobState::Failed);

        // The scheduler must still be fully usable after a worker panic.
        let healthy = scheduler.submit(|_token, _progress| Ok::<_, crate::Error>("still alive"));
        assert_eq!(healthy.join().unwrap(), "still alive");
        assert_eq!(healthy.state(), JobState::Done);
    }

    #[test]
    fn scheduler_marks_erroring_job_failed() {
        let scheduler = JobScheduler::with_threads(1).unwrap();
        let handle =
            scheduler.submit(|_token, _progress| Err::<(), _>(crate::Error::Other("nope".into())));
        assert!(matches!(handle.join(), Err(crate::Error::Other(_))));
        assert_eq!(handle.state(), JobState::Failed);
    }

    #[test]
    fn try_result_is_none_until_done_then_some_once() {
        let scheduler = JobScheduler::with_threads(1).unwrap();
        let handle = scheduler.submit(|_token, _progress| Ok::<_, crate::Error>(7));

        // Block until it's actually finished via state polling, without consuming the
        // result, to exercise try_result's "not consumed yet" -> "consumed" transition.
        while handle.state() != JobState::Done {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(handle.try_result().unwrap().unwrap(), 7);
        assert!(handle.try_result().is_none());
    }
}
