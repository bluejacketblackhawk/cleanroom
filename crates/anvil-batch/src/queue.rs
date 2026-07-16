//! Batch queue (04 §S4): submit files against a preset/tier, run them with per-file
//! isolation (one item's failure never sinks the batch) and concurrency scaled to N-1
//! cores, and expose per-item + overall status/progress for a UI to render.
//!
//! Concurrency is admission-controlled: at most `max_concurrency` jobs are ever
//! outstanding on the shared [`JobScheduler`] (itself sized to `max_concurrency`
//! threads) at once. Everything else sits in an ordered pending list so pause/resume/
//! reorder/remove stay meaningful for items that haven't started yet — once a job is
//! dispatched to the scheduler it's running for real and can only be cancelled, not
//! reordered (04 §S4 "queue controls (pause/reorder/remove)").

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anvil_core::job::{CancellationToken, JobHandle, JobScheduler, ProgressReporter};
use anvil_media::AudioBuffer;
use anvil_project::{Preset, Tier};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::catalog::{self, OutputSettings};
use crate::error::BatchError;

/// Identifies one job submitted to a [`BatchQueue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BatchJobId(pub Uuid);

impl BatchJobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BatchJobId {
    fn default() -> Self {
        Self::new()
    }
}

/// One file to master: resolved input/output paths plus the preset/tier to run it with.
/// Built by [`crate::catalog`] (flat file list or back-catalog folder expansion) or the
/// watch engine (one job per stabilized file); consumed by [`BatchQueue::submit_jobs`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchJob {
    pub input: PathBuf,
    pub output: PathBuf,
    pub preset: Preset,
    pub tier: Tier,
}

/// Lifecycle state of one queued job, mirrored to a UI (04 §S4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchItemState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// A point-in-time snapshot of one job's status, safe to serialize straight to a UI (04
/// §S4's batch table: file / status / progress / result link).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchItemStatus {
    pub id: BatchJobId,
    pub input: PathBuf,
    pub output: PathBuf,
    pub state: BatchItemState,
    /// Latest progress fraction in `[0.0, 1.0]` (0 until the job starts running).
    pub progress: f32,
    /// Latest progress message (e.g. "Mastering", "Writing output").
    pub message: String,
    /// Populated once `state` is [`BatchItemState::Failed`].
    pub error: Option<String>,
}

struct ItemRecord {
    job: BatchJob,
    state: BatchItemState,
    progress: f32,
    message: String,
    error: Option<String>,
}

impl ItemRecord {
    fn new(job: BatchJob) -> Self {
        Self {
            job,
            state: BatchItemState::Queued,
            progress: 0.0,
            message: "Queued".into(),
            error: None,
        }
    }

    fn status(&self, id: BatchJobId) -> BatchItemStatus {
        BatchItemStatus {
            id,
            input: self.job.input.clone(),
            output: self.job.output.clone(),
            state: self.state,
            progress: self.progress,
            message: self.message.clone(),
            error: self.error.clone(),
        }
    }
}

/// The unit of work a dispatched job runs. Production code always uses [`render_job`];
/// tests substitute a cheap stand-in via `BatchQueue::with_worker` so concurrency/
/// cancellation behaviour can be asserted without decoding real audio on every job.
type Worker = dyn Fn(&BatchJob, &CancellationToken, &ProgressReporter) -> anvil_core::Result<PathBuf>
    + Send
    + Sync;

#[derive(Default)]
struct State {
    /// Every job ever submitted, in submission order — the order `snapshot` reports in.
    order: Vec<BatchJobId>,
    /// Jobs waiting for a concurrency slot, in dispatch order. A subset of `order`.
    pending: VecDeque<BatchJobId>,
    items: HashMap<BatchJobId, ItemRecord>,
    running: HashMap<BatchJobId, Arc<JobHandle<PathBuf>>>,
    paused: bool,
}

struct Inner {
    scheduler: JobScheduler,
    max_concurrency: usize,
    worker: Arc<Worker>,
    state: Mutex<State>,
    cv: Condvar,
    /// Dispatcher exit flag. Every store must happen while holding `state` (see `Drop
    /// for BatchQueue`), or the dispatcher can miss the wakeup and park forever.
    shutdown: AtomicBool,
}

/// A batch queue: files go in via `submit_*`, a background dispatcher runs them against
/// [`anvil_dsp::master`] with concurrency capped at construction time, and callers poll
/// [`BatchQueue::snapshot`] / [`BatchQueue::status`] (or subscribe to a specific job's own
/// progress channel via the scheduler, for finer-grained UI updates) for results.
///
/// Cheap to share: clone the `Arc` you wrap it in, or just hand out `&BatchQueue` — every
/// method takes `&self`.
pub struct BatchQueue {
    inner: Arc<Inner>,
    dispatcher: Option<thread::JoinHandle<()>>,
}

impl BatchQueue {
    /// A queue whose concurrency is scaled to N-1 logical cores (04 §S4 "concurrency
    /// auto"), minimum 1 so a single-core box still makes progress.
    pub fn new() -> anvil_core::Result<Self> {
        let n = num_cpus::get().saturating_sub(1).max(1);
        Self::with_concurrency(n)
    }

    /// A queue with an explicit concurrency cap (e.g. for tests, or a user override of
    /// the "auto" default).
    pub fn with_concurrency(max_concurrency: usize) -> anvil_core::Result<Self> {
        Self::build(max_concurrency, Arc::new(render_job))
    }

    /// Test-only constructor: same engine, but with the per-job work function swapped
    /// out so concurrency/cancellation/isolation can be exercised deterministically and
    /// cheaply, without depending on `anvil_dsp::master`'s real runtime.
    #[cfg(test)]
    fn with_worker(max_concurrency: usize, worker: Arc<Worker>) -> anvil_core::Result<Self> {
        Self::build(max_concurrency, worker)
    }

    fn build(max_concurrency: usize, worker: Arc<Worker>) -> anvil_core::Result<Self> {
        let max_concurrency = max_concurrency.max(1);
        let scheduler = JobScheduler::with_threads(max_concurrency)?;
        let inner = Arc::new(Inner {
            scheduler,
            max_concurrency,
            worker,
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let dispatcher = spawn_dispatcher(Arc::clone(&inner));
        Ok(Self {
            inner,
            dispatcher: Some(dispatcher),
        })
    }

    /// Submit pre-resolved jobs directly — the primitive both convenience methods below
    /// and the watch engine build on. Returns the ids assigned, in the same order.
    pub fn submit_jobs(&self, jobs: Vec<BatchJob>) -> Vec<BatchJobId> {
        let mut state = self.inner.state.lock();
        let mut ids = Vec::with_capacity(jobs.len());
        for job in jobs {
            let id = BatchJobId::new();
            state.order.push(id);
            state.items.insert(id, ItemRecord::new(job));
            state.pending.push_back(id);
            ids.push(id);
        }
        drop(state);
        self.inner.cv.notify_all();
        ids
    }

    /// Submit a flat file list under one preset/tier/output settings (04 §S4's plain
    /// batch: drop N files, pick a preset, go).
    pub fn submit_files(
        &self,
        inputs: Vec<PathBuf>,
        preset: Preset,
        tier: Tier,
        output: &OutputSettings,
    ) -> Vec<BatchJobId> {
        self.submit_jobs(catalog::flat_targets(inputs, &preset, tier, output))
    }

    /// Recurse `root` and submit every matching file (back-catalog mode, 04 §S4),
    /// honoring [`OutputSettings::preserve_structure`].
    pub fn submit_folder(
        &self,
        root: &Path,
        preset: Preset,
        tier: Tier,
        output: &OutputSettings,
    ) -> anvil_core::Result<Vec<BatchJobId>> {
        let jobs = catalog::folder_targets(root, &preset, tier, output)?;
        Ok(self.submit_jobs(jobs))
    }

    /// Snapshot of one job's status, or `None` if `id` is unknown (never submitted, or
    /// already [`BatchQueue::remove`]d).
    pub fn status(&self, id: BatchJobId) -> Option<BatchItemStatus> {
        let state = self.inner.state.lock();
        state.items.get(&id).map(|r| r.status(id))
    }

    /// Every job's status, in submission order — the source for the S4 batch table.
    pub fn snapshot(&self) -> Vec<BatchItemStatus> {
        let state = self.inner.state.lock();
        state
            .order
            .iter()
            .filter_map(|id| state.items.get(id).map(|r| r.status(*id)))
            .collect()
    }

    /// Overall progress across every job ever submitted: 0.0 with nothing submitted yet
    /// isn't meaningful, so an empty queue reports 1.0 (nothing left to do). Terminal
    /// jobs (done/failed/cancelled) count as 1.0 each; running jobs count their own
    /// fractional progress; queued jobs count 0.0.
    pub fn overall_progress(&self) -> f32 {
        let state = self.inner.state.lock();
        if state.items.is_empty() {
            return 1.0;
        }
        let sum: f32 = state
            .items
            .values()
            .map(|r| match r.state {
                BatchItemState::Done | BatchItemState::Failed | BatchItemState::Cancelled => 1.0,
                BatchItemState::Running => r.progress,
                BatchItemState::Queued => 0.0,
            })
            .sum();
        sum / state.items.len() as f32
    }

    /// Cancel one job: if it's still pending, it's removed from the queue and marked
    /// cancelled immediately without ever starting; if it's running, cooperative
    /// cancellation is requested (the job stops at its next checkpoint — see
    /// [`render_job`]). Returns `false` if `id` is unknown or already terminal.
    pub fn cancel(&self, id: BatchJobId) -> bool {
        let mut state = self.inner.state.lock();
        if let Some(handle) = state.running.get(&id) {
            handle.cancel();
            return true;
        }
        if let Some(pos) = state.pending.iter().position(|x| *x == id) {
            state.pending.remove(pos);
            mark_cancelled(&mut state, id);
            drop(state);
            self.inner.cv.notify_all();
            return true;
        }
        false
    }

    /// Cancel every pending and running job (the S4 "overall cancel"). Already-finished
    /// jobs are left alone.
    pub fn cancel_all(&self) {
        let mut state = self.inner.state.lock();
        for handle in state.running.values() {
            handle.cancel();
        }
        let pending: Vec<BatchJobId> = state.pending.drain(..).collect();
        for id in pending {
            mark_cancelled(&mut state, id);
        }
        drop(state);
        self.inner.cv.notify_all();
    }

    /// Stop dispatching new jobs. Already-running jobs keep going; nothing new starts
    /// until [`BatchQueue::resume`].
    pub fn pause(&self) {
        self.inner.state.lock().paused = true;
    }

    /// Resume dispatching from the pending queue.
    pub fn resume(&self) {
        let mut state = self.inner.state.lock();
        state.paused = false;
        drop(state);
        self.inner.cv.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        self.inner.state.lock().paused
    }

    /// Move a still-pending job to `new_index` within the dispatch order. Returns
    /// `false` if `id` isn't pending (already running/finished, or unknown) — reordering
    /// a job that has already started isn't meaningful (04 §S4 "reorder", "where
    /// practical").
    pub fn reorder(&self, id: BatchJobId, new_index: usize) -> bool {
        let mut state = self.inner.state.lock();
        if let Some(pos) = state.pending.iter().position(|x| *x == id) {
            let item = state.pending.remove(pos).expect("position just found");
            let new_index = new_index.min(state.pending.len());
            state.pending.insert(new_index, item);
            true
        } else {
            false
        }
    }

    /// Remove a job from the queue entirely: cancels it if running, drops it if pending,
    /// or just forgets it if already finished. Returns `false` if `id` was never known.
    pub fn remove(&self, id: BatchJobId) -> bool {
        let mut state = self.inner.state.lock();
        let existed = state.items.remove(&id).is_some();
        if let Some(pos) = state.pending.iter().position(|x| *x == id) {
            state.pending.remove(pos);
        }
        if let Some(handle) = state.running.remove(&id) {
            handle.cancel();
        }
        state.order.retain(|x| *x != id);
        drop(state);
        self.inner.cv.notify_all();
        existed
    }

    /// Re-queue every job currently [`BatchItemState::Failed`] (04 §S4 "one-click retry
    /// failed"). Returns the ids re-queued.
    pub fn retry_failed(&self) -> Vec<BatchJobId> {
        let mut state = self.inner.state.lock();
        let failed: Vec<BatchJobId> = state
            .order
            .iter()
            .copied()
            .filter(|id| {
                state
                    .items
                    .get(id)
                    .is_some_and(|r| r.state == BatchItemState::Failed)
            })
            .collect();
        for id in &failed {
            if let Some(record) = state.items.get_mut(id) {
                record.state = BatchItemState::Queued;
                record.progress = 0.0;
                record.message = "Queued".into();
                record.error = None;
            }
            state.pending.push_back(*id);
        }
        drop(state);
        if !failed.is_empty() {
            self.inner.cv.notify_all();
        }
        failed
    }

    /// Block until every submitted job reaches a terminal state or `timeout` elapses.
    /// Returns `true` if the queue drained. Mainly for a synchronous driver (the CLI's
    /// `anvil batch`) or tests; a UI should prefer polling [`BatchQueue::snapshot`]
    /// instead of blocking the calling thread.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let done = {
                let state = self.inner.state.lock();
                state.items.values().all(|r| {
                    matches!(
                        r.state,
                        BatchItemState::Done | BatchItemState::Failed | BatchItemState::Cancelled
                    )
                })
            };
            if done {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for BatchQueue {
    fn drop(&mut self) {
        // The lock looks redundant around an atomic store, but it's load-bearing: the
        // dispatcher checks `shutdown` and parks on `cv` atomically under `state`, so a
        // store+notify done without the lock can land in between — a lost wakeup that
        // parks the dispatcher forever and hangs the `join` below.
        {
            let _state = self.inner.state.lock();
            self.inner.shutdown.store(true, Ordering::SeqCst);
            self.inner.cv.notify_all();
        }
        if let Some(handle) = self.dispatcher.take() {
            let _ = handle.join();
        }
    }
}

fn mark_cancelled(state: &mut State, id: BatchJobId) {
    if let Some(record) = state.items.get_mut(&id) {
        record.state = BatchItemState::Cancelled;
        record.error = Some("cancelled".into());
    }
}

/// Background loop: waits for a free concurrency slot and a pending job, dispatches it,
/// repeats. Exits once [`Inner::shutdown`] is set (on [`BatchQueue`] drop).
fn spawn_dispatcher(inner: Arc<Inner>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("anvil-batch-dispatch".into())
        .spawn(move || dispatcher_loop(inner))
        .expect("failed to spawn batch dispatcher thread")
}

fn dispatcher_loop(inner: Arc<Inner>) {
    loop {
        let (id, job) = {
            let mut state = inner.state.lock();
            loop {
                if inner.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                if !state.paused
                    && state.running.len() < inner.max_concurrency
                    && !state.pending.is_empty()
                {
                    break;
                }
                inner.cv.wait(&mut state);
            }
            let id = state.pending.pop_front().expect("checked non-empty above");
            let job = state
                .items
                .get_mut(&id)
                .expect("pending id always has a record")
                .job
                .clone();
            state.items.get_mut(&id).unwrap().state = BatchItemState::Running;
            (id, job)
        };
        dispatch_job(&inner, id, job);
    }
}

/// Hand one job to the [`JobScheduler`], then spawn a lightweight watcher thread that
/// drains its progress stream and folds the final result back into queue state. The
/// watcher's `join()` blocks a plain OS thread (not a scheduler worker), so it never
/// steals a concurrency slot from other jobs.
fn dispatch_job(inner: &Arc<Inner>, id: BatchJobId, job: BatchJob) {
    let worker = Arc::clone(&inner.worker);
    let handle = Arc::new(
        inner
            .scheduler
            .submit(move |token, progress| worker(&job, token, progress)),
    );

    {
        let mut state = inner.state.lock();
        state.running.insert(id, Arc::clone(&handle));
    }

    let watcher_inner = Arc::clone(inner);
    thread::spawn(move || {
        // `progress_rx.iter()` blocks until the job's `ProgressReporter` is dropped,
        // which happens exactly when the job closure returns — so this loop naturally
        // ends right as the job finishes, with no polling.
        for p in handle.progress().iter() {
            let mut state = watcher_inner.state.lock();
            if let Some(record) = state.items.get_mut(&id) {
                record.progress = p.fraction;
                record.message = p.message;
            }
        }
        let result = handle.join();

        let mut state = watcher_inner.state.lock();
        state.running.remove(&id);
        if let Some(record) = state.items.get_mut(&id) {
            match result {
                Ok(_output) => {
                    record.state = BatchItemState::Done;
                    record.progress = 1.0;
                    record.message = "Done".into();
                    record.error = None;
                }
                Err(anvil_core::Error::Cancelled) => {
                    record.state = BatchItemState::Cancelled;
                    record.error = Some("cancelled".into());
                }
                Err(e) => {
                    record.state = BatchItemState::Failed;
                    record.error = Some(e.to_string());
                }
            }
        }
        drop(state);
        // A slot just freed up (or a pause/resume/reorder happened while we were
        // waiting) — wake the dispatcher so it can pick the next pending job.
        watcher_inner.cv.notify_all();
    });
}

/// Render one job: analyze+master via `anvil_dsp::master`, then write the result to
/// disk. Cancellation is checked at the two block boundaries around the (currently
/// non-interruptible) synchronous `master` call — a job cancelled before it starts never
/// runs; one cancelled while `master` is mid-flight still finishes that CPU-bound call
/// but is discarded (no output written) rather than left running to completion silently.
///
/// --- ENCODER SEAM ---------------------------------------------------------------------
/// Interim: 16-bit PCM WAV via `hound`, the only output format batch/watch can produce
/// today. `anvil_media::encode` is landing in parallel (M2 lane A); once it's available,
/// replace the `write_wav_16bit` call below with a call into it so batch/watch outputs
/// gain the full format matrix (MP3/Opus/Vorbis/FLAC/ALAC/AAC) instead of being
/// hardcoded to WAV. Nothing else in this module needs to change — `render_job` is the
/// only place that writes rendered audio to disk.
/// ----------------------------------------------------------------------------------------
fn render_job(
    job: &BatchJob,
    token: &CancellationToken,
    progress: &ProgressReporter,
) -> anvil_core::Result<PathBuf> {
    token.check()?;
    progress.report(0.05, "Mastering");

    let result = anvil_dsp::master(&job.input, &job.preset, job.tier).map_err(BatchError::from)?;

    token.check()?;
    progress.report(0.9, "Writing output");

    if let Some(parent) = job.output.parent() {
        std::fs::create_dir_all(parent).map_err(BatchError::from)?;
    }
    write_wav_16bit(&job.output, &result.audio)?;

    progress.report(1.0, "Done");
    Ok(job.output.clone())
}

fn write_wav_16bit(path: &Path, audio: &AudioBuffer) -> anvil_core::Result<()> {
    let channels = audio.channel_count().max(1);
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: audio.sample_rate(),
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(BatchError::from)?;
    for frame in 0..audio.frames() {
        for ch in 0..channels {
            let s = audio.channel(ch).get(frame).copied().unwrap_or(0.0);
            let q = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            writer.write_sample(q).map_err(BatchError::from)?;
        }
    }
    writer.finalize().map_err(BatchError::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn fixture_wav(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..48_000u32 {
            let s = (0.2 * ((i as f32) * 0.05).sin() * i16::MAX as f32) as i16;
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    fn job(input: PathBuf, output: PathBuf) -> BatchJob {
        BatchJob {
            input,
            output,
            preset: Preset::default(),
            tier: Tier::Fast,
        }
    }

    #[test]
    fn per_file_isolation_one_bad_input_does_not_sink_others() {
        let tmp = tempfile::tempdir().unwrap();
        let good_a = fixture_wav(tmp.path(), "a.wav");
        let good_b = fixture_wav(tmp.path(), "b.wav");
        let bad = tmp.path().join("missing.wav"); // never written -> decode fails

        let queue = BatchQueue::with_concurrency(2).unwrap();
        let ids = queue.submit_jobs(vec![
            job(good_a, tmp.path().join("a_out.wav")),
            job(bad, tmp.path().join("bad_out.wav")),
            job(good_b, tmp.path().join("b_out.wav")),
        ]);
        assert!(queue.wait_idle(Duration::from_secs(30)));

        let statuses: Vec<_> = ids.iter().map(|id| queue.status(*id).unwrap()).collect();
        assert_eq!(statuses[0].state, BatchItemState::Done);
        assert_eq!(statuses[1].state, BatchItemState::Failed);
        assert!(statuses[1].error.is_some());
        assert_eq!(statuses[2].state, BatchItemState::Done);
        assert!(statuses[0].output.exists());
        assert!(statuses[2].output.exists());
    }

    #[test]
    fn concurrency_is_respected() {
        let concurrency = 2usize;
        let current = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&current);
        let m1 = Arc::clone(&max_seen);
        let worker: Arc<Worker> = Arc::new(move |job: &BatchJob, token, _progress| {
            let now = c1.fetch_add(1, Ordering::SeqCst) + 1;
            m1.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(40));
            c1.fetch_sub(1, Ordering::SeqCst);
            token.check()?;
            Ok(job.output.clone())
        });

        let queue = BatchQueue::with_worker(concurrency, worker).unwrap();
        let jobs: Vec<BatchJob> = (0..6)
            .map(|i| {
                job(
                    PathBuf::from(format!("in{i}.wav")),
                    PathBuf::from(format!("out{i}.wav")),
                )
            })
            .collect();
        queue.submit_jobs(jobs);
        assert!(queue.wait_idle(Duration::from_secs(10)));

        assert!(max_seen.load(Ordering::SeqCst) <= concurrency);
        assert_eq!(max_seen.load(Ordering::SeqCst), concurrency);
    }

    #[test]
    fn cancel_all_stops_pending_items_before_they_run() {
        let concurrency = 1usize;
        let started = Arc::new(AtomicUsize::new(0));
        let block = Arc::new(std::sync::Barrier::new(2));

        let s1 = Arc::clone(&started);
        let b1 = Arc::clone(&block);
        let worker: Arc<Worker> = Arc::new(move |job, token, _progress| {
            s1.fetch_add(1, Ordering::SeqCst);
            b1.wait(); // hold the only concurrency slot until the test releases it
            token.check()?;
            Ok(job.output.clone())
        });

        let queue = BatchQueue::with_worker(concurrency, worker).unwrap();
        let jobs: Vec<BatchJob> = (0..5)
            .map(|i| {
                job(
                    PathBuf::from(format!("in{i}.wav")),
                    PathBuf::from(format!("out{i}.wav")),
                )
            })
            .collect();
        let ids = queue.submit_jobs(jobs);

        // Wait for exactly the first job to grab the only slot and block inside it.
        while started.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(1));
        }

        queue.cancel_all();
        // Release the running job so the queue can settle.
        block.wait();

        assert!(queue.wait_idle(Duration::from_secs(10)));
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "pending jobs must never start after cancel_all"
        );

        let statuses: Vec<_> = ids.iter().map(|id| queue.status(*id).unwrap()).collect();
        // Job 0 was running and cooperatively cancelled; jobs 1..4 were still pending
        // and were cancelled without ever starting.
        assert!(statuses
            .iter()
            .all(|s| s.state == BatchItemState::Cancelled));
    }

    #[test]
    fn reorder_moves_a_pending_item() {
        let block = Arc::new(std::sync::Barrier::new(2));
        let b1 = Arc::clone(&block);
        let worker: Arc<Worker> = Arc::new(move |job, _token, _progress| {
            b1.wait();
            Ok(job.output.clone())
        });
        let queue = BatchQueue::with_worker(1, worker).unwrap();
        let ids = queue.submit_jobs(vec![
            job(PathBuf::from("a"), PathBuf::from("a_out")),
            job(PathBuf::from("b"), PathBuf::from("b_out")),
            job(PathBuf::from("c"), PathBuf::from("c_out")),
        ]);
        // First job (a) is immediately picked up and blocks on the barrier; b and c are
        // still pending. Move c to the front of the pending list.
        thread::sleep(Duration::from_millis(20));
        assert!(queue.reorder(ids[2], 0));
        block.wait(); // release "a"
        block.wait(); // release whichever runs next
        block.wait(); // release the last one
        assert!(queue.wait_idle(Duration::from_secs(10)));
    }

    #[test]
    fn remove_drops_a_pending_item_without_running_it() {
        let started = Arc::new(AtomicUsize::new(0));
        let s1 = Arc::clone(&started);
        let block = Arc::new(std::sync::Barrier::new(2));
        let b1 = Arc::clone(&block);
        let worker: Arc<Worker> = Arc::new(move |job, _token, _progress| {
            s1.fetch_add(1, Ordering::SeqCst);
            b1.wait();
            Ok(job.output.clone())
        });
        let queue = BatchQueue::with_worker(1, worker).unwrap();
        let ids = queue.submit_jobs(vec![
            job(PathBuf::from("a"), PathBuf::from("a_out")),
            job(PathBuf::from("b"), PathBuf::from("b_out")),
        ]);
        thread::sleep(Duration::from_millis(20)); // let "a" grab the only slot
        assert!(queue.remove(ids[1]));
        assert!(queue.status(ids[1]).is_none());
        block.wait(); // release "a"
        assert!(queue.wait_idle(Duration::from_secs(10)));
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_failed_requeues_only_failed_items() {
        let attempt = Arc::new(AtomicUsize::new(0));
        let a1 = Arc::clone(&attempt);
        let worker: Arc<Worker> = Arc::new(move |job, _token, _progress| {
            let n = a1.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(anvil_core::Error::Other("boom".into()))
            } else {
                Ok(job.output.clone())
            }
        });
        let queue = BatchQueue::with_worker(1, worker).unwrap();
        let ids = queue.submit_jobs(vec![job(PathBuf::from("a"), PathBuf::from("a_out"))]);
        assert!(queue.wait_idle(Duration::from_secs(10)));
        assert_eq!(queue.status(ids[0]).unwrap().state, BatchItemState::Failed);

        let retried = queue.retry_failed();
        assert_eq!(retried, ids);
        assert!(queue.wait_idle(Duration::from_secs(10)));
        assert_eq!(queue.status(ids[0]).unwrap().state, BatchItemState::Done);
    }

    #[test]
    fn overall_progress_reaches_one_when_drained() {
        let worker: Arc<Worker> = Arc::new(|job, _token, _progress| Ok(job.output.clone()));
        let queue = BatchQueue::with_worker(2, worker).unwrap();
        queue.submit_jobs(vec![
            job(PathBuf::from("a"), PathBuf::from("a_out")),
            job(PathBuf::from("b"), PathBuf::from("b_out")),
        ]);
        assert!(queue.wait_idle(Duration::from_secs(10)));
        assert_eq!(queue.overall_progress(), 1.0);
    }

    /// Regression test for a missed-wakeup deadlock: `Drop` once stored `shutdown` and
    /// notified without holding the state lock, so both could land between the
    /// dispatcher's shutdown check and its `cv.wait` — the notify reached no waiter, the
    /// dispatcher parked forever, and drop hung in `join()`. Dropping right after
    /// construction (no jobs, dispatcher headed straight for the wait) maximizes that
    /// window; enough iterations make a reintroduction show up as a hang here.
    #[test]
    fn construct_then_drop_immediately_does_not_hang() {
        for _ in 0..300 {
            let queue = BatchQueue::with_concurrency(1).unwrap();
            drop(queue);
        }
    }
}
