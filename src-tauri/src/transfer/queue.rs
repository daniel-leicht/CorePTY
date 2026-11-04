//! The transfer queue: runs engine jobs in the background with a concurrency
//! limit, reports throttled progress, and routes conflict questions to the UI.

use super::engine::{
    Conflict, ConflictAnswer, ConflictResolver, Counters, Engine, Resolution, Side, Snapshot,
    TransferError, TransferKind, WorkStats,
};
use crate::vfs::{FsId, Location, LocationView, VPath, Vfs};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

/// Jobs running at once, across all filesystems.
const MAX_RUNNING: usize = 2;
/// Finished jobs kept for `transfer_list`.
const KEEP_FINISHED: usize = 50;
/// At most ten progress events a second per job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// How long a cancelled job gets to clean up (remove its partial file)
/// before it's abandoned: a stalled server may never answer.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequest {
    pub kind: TransferKind,
    pub sources: Vec<Location>,
    /// The folder the sources go into.
    pub destination: Location,
    /// Give copies the source's permissions (POSIX mode bits). On unless
    /// the request says otherwise.
    #[serde(default = "keep")]
    pub preserve_mode: bool,
}

fn keep() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Planning,
    Running,
    WaitingForUser,
    /// A filesystem it uses is disconnected; it carries on after a reconnect.
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    fn finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TransferEvent {
    Queued {
        id: String,
    },
    Planning {
        id: String,
    },
    Planned {
        id: String,
        total_items: u64,
        total_bytes: u64,
    },
    Progress {
        id: String,
        current: String,
        completed_items: u64,
        completed_bytes: u64,
        total_items: u64,
        total_bytes: u64,
        skipped_items: u64,
        bytes_per_second: u64,
        eta_seconds: Option<u64>,
    },
    /// Answer with `transfer_resolve(id, conflictId, answer)`.
    Conflict {
        id: String,
        conflict_id: u64,
        conflict: Conflict,
    },
    Finished {
        id: String,
        completed_items: u64,
        completed_bytes: u64,
        skipped_items: u64,
    },
    Failed {
        id: String,
        message: String,
    },
    Cancelled {
        id: String,
    },
    /// Waiting for a connection to come back (`fs` is the filesystem id).
    Paused {
        id: String,
        fs: FsId,
        message: String,
    },
}

pub type EventSink = Arc<dyn Fn(TransferEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub id: String,
    pub kind: TransferKind,
    pub state: JobState,
    pub message: Option<String>,
    /// The first source; `source_count` says how many there are.
    pub source: LocationView,
    pub source_count: usize,
    pub destination: LocationView,
    pub total_items: u64,
    pub total_bytes: u64,
    pub completed_items: u64,
    pub completed_bytes: u64,
    pub skipped_items: u64,
}

struct Job {
    id: String,
    kind: TransferKind,
    preserve_mode: bool,
    sources: Vec<Location>,
    destination: Location,
    state: Mutex<JobState>,
    message: Mutex<Option<String>>,
    cancel: watch::Sender<bool>,
    counters: Arc<Counters>,
    totals: Mutex<WorkStats>,
    /// The conflict awaiting an answer, if any.
    pending: Mutex<Option<(u64, oneshot::Sender<ConflictAnswer>)>>,
    conflict_ids: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Job {
    fn state(&self) -> JobState {
        *lock(&self.state)
    }

    fn set_state(&self, state: JobState) {
        *lock(&self.state) = state;
    }

    fn view(&self) -> JobView {
        let totals = *lock(&self.totals);
        let snapshot = self.counters.snapshot();
        JobView {
            id: self.id.clone(),
            kind: self.kind,
            state: self.state(),
            message: lock(&self.message).clone(),
            source: self.sources[0].view(),
            source_count: self.sources.len(),
            destination: self.destination.view(),
            total_items: totals.items,
            total_bytes: totals.bytes,
            completed_items: snapshot.done.items,
            completed_bytes: snapshot.done.bytes,
            skipped_items: snapshot.skipped,
        }
    }
}

#[derive(Default)]
struct Inner {
    jobs: Mutex<Vec<Arc<Job>>>,
    remote_slots: Mutex<HashMap<FsId, Arc<Semaphore>>>,
}

/// Tauri-managed state: every transfer job of this app session.
pub struct TransferManager {
    inner: Arc<Inner>,
    running: Arc<Semaphore>,
}

impl Default for TransferManager {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            running: Arc::new(Semaphore::new(MAX_RUNNING)),
        }
    }
}

impl TransferManager {
    /// Queue a job; it runs in the background and reports through `sink`.
    pub fn start(
        &self,
        request: TransferRequest,
        source: Arc<dyn Vfs>,
        target: Arc<dyn Vfs>,
        sink: EventSink,
    ) -> Result<JobView, String> {
        let source_fs = match request.sources.first() {
            Some(first) => first.fs.clone(),
            None => return Err("select at least one file or folder".to_string()),
        };
        if request
            .sources
            .iter()
            .any(|location| location.fs != source_fs)
        {
            return Err("all items in one transfer must come from the same place".to_string());
        }
        let (cancel, _) = watch::channel(false);
        let job = Arc::new(Job {
            id: Uuid::new_v4().to_string(),
            kind: request.kind,
            preserve_mode: request.preserve_mode,
            sources: request.sources,
            destination: request.destination,
            state: Mutex::new(JobState::Queued),
            message: Mutex::new(None),
            cancel,
            counters: Arc::default(),
            totals: Mutex::new(WorkStats::default()),
            pending: Mutex::new(None),
            conflict_ids: AtomicU64::new(0),
        });
        {
            let mut jobs = lock(&self.inner.jobs);
            jobs.push(job.clone());
            prune_finished(&mut jobs);
        }
        let view = job.view();
        let source = Side {
            id: source_fs,
            fs: source,
        };
        let target = Side {
            id: job.destination.fs.clone(),
            fs: target,
        };
        tauri::async_runtime::spawn(run_job(
            self.inner.clone(),
            self.running.clone(),
            job,
            source,
            target,
            sink,
        ));
        Ok(view)
    }

    pub fn cancel(&self, id: &str) -> bool {
        match self.find(id) {
            Some(job) if !job.state().finished() => {
                job.cancel.send_replace(true);
                true
            }
            _ => false,
        }
    }

    pub fn list(&self) -> Vec<JobView> {
        lock(&self.inner.jobs)
            .iter()
            .map(|job| job.view())
            .collect()
    }

    pub fn resolve(
        &self,
        id: &str,
        conflict_id: u64,
        answer: ConflictAnswer,
    ) -> Result<(), String> {
        let job = self.find(id).ok_or_else(|| format!("no transfer {id}"))?;
        let mut pending = lock(&job.pending);
        match pending.take() {
            Some((pending_id, reply)) if pending_id == conflict_id => {
                let _ = reply.send(answer);
                Ok(())
            }
            other => {
                *pending = other;
                Err("that question was already answered".to_string())
            }
        }
    }

    fn find(&self, id: &str) -> Option<Arc<Job>> {
        lock(&self.inner.jobs)
            .iter()
            .find(|job| job.id == id)
            .cloned()
    }
}

fn prune_finished(jobs: &mut Vec<Arc<Job>>) {
    let finished = jobs.iter().filter(|job| job.state().finished()).count();
    let mut excess = finished.saturating_sub(KEEP_FINISHED);
    jobs.retain(|job| {
        if excess > 0 && job.state().finished() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

/// Reserve a slot: one job at a time per remote connection (servers dislike
/// parallel sessions), then one of the global slots. Remote slots are taken
/// first and in sorted order, so waiting jobs neither deadlock nor hold a
/// global slot while they wait.
async fn reserve(
    inner: &Inner,
    running: Arc<Semaphore>,
    job: &Job,
    source_fs: &FsId,
) -> Vec<OwnedSemaphorePermit> {
    let mut remotes: Vec<FsId> = [source_fs.clone(), job.destination.fs.clone()]
        .into_iter()
        .filter(|fs| !fs.is_local())
        .collect();
    remotes.sort();
    remotes.dedup();
    let mut permits = Vec::new();
    for fs in remotes {
        let slot = lock(&inner.remote_slots)
            .entry(fs)
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone();
        permits.push(
            slot.acquire_owned()
                .await
                .expect("transfer slots are never closed"),
        );
    }
    permits.push(
        running
            .acquire_owned()
            .await
            .expect("transfer slots are never closed"),
    );
    permits
}

async fn run_job(
    inner: Arc<Inner>,
    running: Arc<Semaphore>,
    job: Arc<Job>,
    source: Side,
    target: Side,
    sink: EventSink,
) {
    let id = job.id.clone();
    sink(TransferEvent::Queued { id: id.clone() });
    let mut cancelled = job.cancel.subscribe();

    let resolver = Arc::new(QueueResolver {
        job: job.clone(),
        sink: sink.clone(),
    });
    let mut engine = Engine::new(
        job.kind,
        source.clone(),
        target.clone(),
        job.counters.clone(),
        job.cancel.subscribe(),
        resolver,
    );
    engine.set_preserve_mode(job.preserve_mode);
    let sides = [source, target];
    let sources: Vec<VPath> = job
        .sources
        .iter()
        .map(|location| location.path.clone())
        .collect();

    let result = loop {
        if let Err(error) = until_connected(&job, &sink, &sides, &mut cancelled).await {
            break Err(error);
        }
        if job.state() == JobState::Paused {
            job.set_state(JobState::Queued);
            sink(TransferEvent::Queued { id: id.clone() });
        }
        // Slots are held only while running: a paused job lets others go.
        let permits = tokio::select! {
            permits = reserve(&inner, running.clone(), &job, &sides[0].id) => permits,
            _ = cancelled.wait_for(|cancelled| *cancelled) => break Err(TransferError::Cancelled),
        };
        if sides.iter().any(|side| !side.fs.is_connected()) {
            // It dropped again while this job waited for a slot.
            continue;
        }
        let attempt = attempt(&job, &sink, &mut engine, &sources).await;
        drop(permits);
        match attempt {
            // The connection dropped mid-way: wait for it, then pick up
            // where it stopped (finished files kept, partial ones resumed).
            Err(TransferError::Failed(_)) if sides.iter().any(|side| !side.fs.is_connected()) => {
                job.counters.reset();
            }
            other => break other,
        }
    };
    finish(&job, &sink, result);
    prune_finished(&mut lock(&inner.jobs));
}

/// Wait while a filesystem the job uses is disconnected (paused).
async fn until_connected(
    job: &Job,
    sink: &EventSink,
    sides: &[Side; 2],
    cancelled: &mut watch::Receiver<bool>,
) -> Result<(), TransferError> {
    for side in sides {
        let Some(mut connection) = side.fs.connection() else {
            continue;
        };
        while !side.fs.is_connected() {
            if job.state() != JobState::Paused {
                job.set_state(JobState::Paused);
                sink(TransferEvent::Paused {
                    id: job.id.clone(),
                    fs: side.id.clone(),
                    message: "the server is disconnected — waiting to reconnect".to_string(),
                });
            }
            tokio::select! {
                changed = connection.changed() => {
                    if changed.is_err() {
                        return Err(TransferError::Failed(format!("{} was closed", side.id.0)));
                    }
                }
                _ = cancelled.wait_for(|cancelled| *cancelled) => return Err(TransferError::Cancelled),
            }
        }
    }
    Ok(())
}

/// Plan and run the job once, reporting progress. Cancelling stops it even
/// while a request hangs: the engine gets a moment to clean up, then it's
/// dropped (which drops whatever it was waiting for).
async fn attempt(
    job: &Arc<Job>,
    sink: &EventSink,
    engine: &mut Engine,
    sources: &[VPath],
) -> Result<(), TransferError> {
    let mut cancelled = job.cancel.subscribe();
    let run = attempt_once(job, sink, engine, sources);
    tokio::pin!(run);
    let cancel = async {
        let _ = cancelled.wait_for(|cancelled| *cancelled).await;
    };
    tokio::select! {
        result = &mut run => result,
        () = cancel => {
            let _ = tokio::time::timeout(CANCEL_GRACE, run).await;
            Err(TransferError::Cancelled)
        }
    }
}

async fn attempt_once(
    job: &Arc<Job>,
    sink: &EventSink,
    engine: &mut Engine,
    sources: &[VPath],
) -> Result<(), TransferError> {
    let id = job.id.clone();
    job.set_state(JobState::Planning);
    sink(TransferEvent::Planning { id: id.clone() });
    {
        let totals = engine.plan(sources).await?;
        *lock(&job.totals) = totals;
        sink(TransferEvent::Planned {
            id: id.clone(),
            total_items: totals.items,
            total_bytes: totals.bytes,
        });
        job.set_state(JobState::Running);

        let run = engine.run(sources, &job.destination.path);
        tokio::pin!(run);
        let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut meter = SpeedMeter::default();
        let mut last_sent: Option<Snapshot> = None;
        let mut last_speed = 0.0;
        loop {
            tokio::select! {
                result = &mut run => break result,
                _ = ticker.tick() => {
                    let snapshot = job.counters.snapshot();
                    // Sample every tick, so the speed decays while a transfer stalls.
                    let speed = meter.sample(snapshot.done.bytes, Instant::now());
                    // Skip when nothing moved and the speed shown has settled
                    // (or a conflict is waiting for the user).
                    let settled = (last_speed - speed).abs() < 1.0 + last_speed * 0.05;
                    if last_sent.as_ref() == Some(&snapshot)
                        && (settled || job.state() == JobState::WaitingForUser)
                    {
                        continue;
                    }
                    sink(progress_event(&id, &snapshot, totals, speed));
                    last_sent = Some(snapshot);
                    last_speed = speed;
                }
            }
        }
    }
}

fn progress_event(id: &str, snapshot: &Snapshot, totals: WorkStats, speed: f64) -> TransferEvent {
    let remaining = totals.bytes.saturating_sub(snapshot.done.bytes);
    TransferEvent::Progress {
        id: id.to_string(),
        current: snapshot.current.clone(),
        completed_items: snapshot.done.items,
        completed_bytes: snapshot.done.bytes,
        total_items: totals.items,
        total_bytes: totals.bytes,
        skipped_items: snapshot.skipped,
        bytes_per_second: speed as u64,
        eta_seconds: (speed >= 1.0).then(|| (remaining as f64 / speed).ceil() as u64),
    }
}

fn finish(job: &Job, sink: &EventSink, result: Result<(), TransferError>) {
    let id = job.id.clone();
    let snapshot = job.counters.snapshot();
    let (state, event) = match result {
        Ok(()) => (
            JobState::Done,
            TransferEvent::Finished {
                id,
                completed_items: snapshot.done.items,
                completed_bytes: snapshot.done.bytes,
                skipped_items: snapshot.skipped,
            },
        ),
        Err(TransferError::Cancelled) => (JobState::Cancelled, TransferEvent::Cancelled { id }),
        Err(TransferError::Failed(message)) => {
            *lock(&job.message) = Some(message.clone());
            (JobState::Failed, TransferEvent::Failed { id, message })
        }
    };
    job.set_state(state);
    sink(event);
}

/// Bytes per second, smoothed so the number doesn't jump on every sample.
#[derive(Default)]
struct SpeedMeter {
    last: Option<(u64, Instant)>,
    rate: f64,
}

impl SpeedMeter {
    fn sample(&mut self, bytes: u64, now: Instant) -> f64 {
        if let Some((last_bytes, last_time)) = self.last {
            let seconds = now.duration_since(last_time).as_secs_f64();
            if seconds > 0.0 {
                let instant = bytes.saturating_sub(last_bytes) as f64 / seconds;
                self.rate = if self.rate == 0.0 {
                    instant
                } else {
                    0.7 * self.rate + 0.3 * instant
                };
            }
        }
        self.last = Some((bytes, now));
        self.rate
    }
}

/// Puts a conflict in front of the user and waits for their answer (or for
/// the job to be cancelled).
struct QueueResolver {
    job: Arc<Job>,
    sink: EventSink,
}

#[async_trait]
impl ConflictResolver for QueueResolver {
    async fn resolve(&self, conflict: Conflict) -> ConflictAnswer {
        let (reply, answer) = oneshot::channel();
        let conflict_id = self.job.conflict_ids.fetch_add(1, Ordering::Relaxed) + 1;
        *lock(&self.job.pending) = Some((conflict_id, reply));
        self.job.set_state(JobState::WaitingForUser);
        (self.sink)(TransferEvent::Conflict {
            id: self.job.id.clone(),
            conflict_id,
            conflict,
        });
        let mut cancelled = self.job.cancel.subscribe();
        let cancel = ConflictAnswer {
            resolution: Resolution::Cancel,
            apply_to_all: false,
        };
        let answer = tokio::select! {
            answer = answer => answer.unwrap_or(cancel),
            _ = cancelled.wait_for(|cancelled| *cancelled) => cancel,
        };
        *lock(&self.job.pending) = None;
        self.job.set_state(JobState::Running);
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::memory::MemFs;
    use tokio::sync::mpsc;

    fn location(fs: &str, path: &str) -> Location {
        Location {
            fs: FsId(fs.to_string()),
            path: MemFs::path(path),
        }
    }

    fn collector() -> (EventSink, mpsc::UnboundedReceiver<TransferEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Arc::new(move |event| drop(sender.send(event))), receiver)
    }

    async fn next(events: &mut mpsc::UnboundedReceiver<TransferEvent>) -> TransferEvent {
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("timed out waiting for a transfer event")
            .expect("the job ended without a final event")
    }

    /// Collect events until the job's final one.
    async fn until_finished(
        events: &mut mpsc::UnboundedReceiver<TransferEvent>,
    ) -> Vec<TransferEvent> {
        let mut seen = Vec::new();
        loop {
            let event = next(events).await;
            let last = matches!(
                event,
                TransferEvent::Finished { .. }
                    | TransferEvent::Failed { .. }
                    | TransferEvent::Cancelled { .. }
            );
            seen.push(event);
            if last {
                return seen;
            }
        }
    }

    fn request(sources: &[&str], destination: &str) -> TransferRequest {
        TransferRequest {
            kind: TransferKind::Copy,
            sources: sources.iter().map(|path| location("m", path)).collect(),
            destination: location("m", destination),
            preserve_mode: true,
        }
    }

    #[tokio::test]
    async fn a_job_reports_its_lifecycle_and_finishes() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/a.txt", b"hello");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();

        let view = manager
            .start(
                request(&["/src/a.txt"], "/dst"),
                fs.clone(),
                fs.clone(),
                sink,
            )
            .unwrap();
        let seen = until_finished(&mut events).await;
        assert!(matches!(seen.first(), Some(TransferEvent::Queued { .. })));
        assert!(seen.iter().any(|e| matches!(
            e,
            TransferEvent::Planned {
                total_items: 1,
                total_bytes: 5,
                ..
            }
        )));
        assert!(matches!(
            seen.last(),
            Some(TransferEvent::Finished {
                completed_bytes: 5,
                ..
            })
        ));
        assert_eq!(fs.read("/dst/a.txt").unwrap(), b"hello");
        let listed = manager.list();
        assert_eq!(listed[0].id, view.id);
        assert_eq!(listed[0].state, JobState::Done);
    }

    #[tokio::test]
    async fn progress_is_throttled() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        // Many files: a per-item event would mean hundreds of events.
        for index in 0..400 {
            fs.add_file(&format!("/src/{index}.bin"), &[0_u8; 1024]);
        }
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let started = Instant::now();
        manager
            .start(request(&["/src"], "/dst"), fs.clone(), fs.clone(), sink)
            .unwrap();
        let seen = until_finished(&mut events).await;
        let progress = seen
            .iter()
            .filter(|e| matches!(e, TransferEvent::Progress { .. }))
            .count();
        let ticks = started.elapsed().as_millis() / PROGRESS_INTERVAL.as_millis() + 2;
        assert!(
            progress as u128 <= ticks,
            "{progress} progress events in {ticks} ticks"
        );
        assert!(matches!(
            seen.last(),
            Some(TransferEvent::Finished {
                completed_items: 401,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn conflicts_are_asked_and_answered_through_the_manager() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/a.txt", b"new");
        fs.add_file("/dst/a.txt", b"old");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let job = manager
            .start(
                request(&["/src/a.txt"], "/dst"),
                fs.clone(),
                fs.clone(),
                sink,
            )
            .unwrap();

        let conflict_id = loop {
            if let TransferEvent::Conflict {
                conflict_id,
                conflict,
                ..
            } = next(&mut events).await
            {
                assert_eq!(conflict.name, "a.txt");
                assert_eq!(conflict.target.size, Some(3));
                break conflict_id;
            }
        };
        assert_eq!(manager.list()[0].state, JobState::WaitingForUser);
        let overwrite = || ConflictAnswer {
            resolution: Resolution::Overwrite,
            apply_to_all: false,
        };
        assert!(
            manager
                .resolve(&job.id, conflict_id + 1, overwrite())
                .is_err(),
            "stale id"
        );
        manager.resolve(&job.id, conflict_id, overwrite()).unwrap();
        assert!(matches!(
            until_finished(&mut events).await.last(),
            Some(TransferEvent::Finished { .. })
        ));
        assert_eq!(fs.read("/dst/a.txt").unwrap(), b"new");
    }

    #[tokio::test]
    async fn cancelling_a_job_that_waits_for_an_answer_stops_it() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/a.txt", b"new");
        fs.add_file("/dst/a.txt", b"old");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let job = manager
            .start(
                request(&["/src/a.txt"], "/dst"),
                fs.clone(),
                fs.clone(),
                sink,
            )
            .unwrap();
        while !matches!(next(&mut events).await, TransferEvent::Conflict { .. }) {}

        assert!(manager.cancel(&job.id));
        assert!(matches!(
            until_finished(&mut events).await.last(),
            Some(TransferEvent::Cancelled { .. })
        ));
        assert_eq!(fs.read("/dst/a.txt").unwrap(), b"old");
        assert_eq!(manager.list()[0].state, JobState::Cancelled);
        assert!(
            !manager.cancel(&job.id),
            "a finished job can't be cancelled"
        );
    }

    #[tokio::test]
    async fn jobs_on_one_remote_run_one_at_a_time() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/a.txt", b"new");
        fs.add_file("/dst/a.txt", b"old");
        fs.add_file("/src/b.txt", b"b");
        let manager = TransferManager::default();
        let (first_sink, mut first) = collector();
        let (second_sink, mut second) = collector();
        let blocked = request(&["/src/a.txt"], "/dst");
        manager
            .start(blocked, fs.clone(), fs.clone(), first_sink)
            .unwrap();
        while !matches!(next(&mut first).await, TransferEvent::Conflict { .. }) {}

        // The first job holds the "m" connection while it waits for an answer.
        let waiting = manager
            .start(
                request(&["/src/b.txt"], "/dst"),
                fs.clone(),
                fs.clone(),
                second_sink,
            )
            .unwrap();
        assert!(matches!(
            next(&mut second).await,
            TransferEvent::Queued { .. }
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            second.try_recv().is_err(),
            "the second job must still be queued"
        );
        assert!(!fs.exists("/dst/b.txt"));

        let first_id = manager.list()[0].id.clone();
        manager.cancel(&first_id);
        assert!(matches!(
            until_finished(&mut second).await.last(),
            Some(TransferEvent::Finished { .. })
        ));
        assert_eq!(fs.read("/dst/b.txt").unwrap(), b"b");
        assert_ne!(waiting.id, first_id);
    }

    #[test]
    fn speed_is_smoothed_and_eta_follows_it() {
        let mut meter = SpeedMeter::default();
        let start = Instant::now();
        assert_eq!(meter.sample(0, start), 0.0);
        let first = meter.sample(1000, start + Duration::from_secs(1));
        assert_eq!(first, 1000.0);
        let second = meter.sample(1000, start + Duration::from_secs(2));
        assert!(
            second > 0.0 && second < 1000.0,
            "a stall lowers the rate gradually"
        );

        let snapshot = Snapshot {
            done: WorkStats {
                items: 1,
                bytes: 1000,
            },
            skipped: 0,
            current: "a".into(),
        };
        let totals = WorkStats {
            items: 2,
            bytes: 3000,
        };
        match progress_event("j", &snapshot, totals, 1000.0) {
            TransferEvent::Progress { eta_seconds, .. } => assert_eq!(eta_seconds, Some(2)),
            other => panic!("unexpected {other:?}"),
        }
    }
    /// A remote filesystem whose connection drops on a given read and stays
    /// down (every call fails) until `reconnect`.
    struct Flaky {
        inner: Arc<MemFs>,
        connected: watch::Sender<bool>,
        reads_before_drop: AtomicU64,
        /// Reads never answer (a server that hangs).
        stall_reads: std::sync::atomic::AtomicBool,
    }

    struct Stalled;

    impl tokio::io::AsyncRead for Stalled {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl Flaky {
        fn new(inner: Arc<MemFs>, reads_before_drop: u64) -> Arc<Self> {
            Arc::new(Self {
                inner,
                connected: watch::channel(true).0,
                reads_before_drop: AtomicU64::new(reads_before_drop),
                stall_reads: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn up(&self) -> std::io::Result<()> {
            if *self.connected.borrow() {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "the connection was lost",
                ))
            }
        }

        fn reconnect(&self) {
            self.connected.send_replace(true);
        }
    }

    #[async_trait]
    impl Vfs for Flaky {
        fn capabilities(&self) -> crate::vfs::Capabilities {
            self.inner.capabilities()
        }
        async fn home(&self) -> std::io::Result<VPath> {
            self.up()?;
            self.inner.home().await
        }
        async fn roots(&self) -> std::io::Result<Vec<crate::vfs::Root>> {
            self.up()?;
            self.inner.roots().await
        }
        async fn resolve(&self, input: &str, base: Option<&VPath>) -> std::io::Result<VPath> {
            self.up()?;
            self.inner.resolve(input, base).await
        }
        async fn canonicalize(&self, path: &VPath) -> std::io::Result<VPath> {
            self.up()?;
            self.inner.canonicalize(path).await
        }
        async fn list(&self, dir: &VPath) -> std::io::Result<Vec<crate::vfs::Entry>> {
            self.up()?;
            self.inner.list(dir).await
        }
        async fn stat(&self, path: &VPath) -> std::io::Result<crate::vfs::Meta> {
            self.up()?;
            self.inner.stat(path).await
        }
        async fn lstat(&self, path: &VPath) -> std::io::Result<crate::vfs::Meta> {
            self.up()?;
            self.inner.lstat(path).await
        }
        async fn mkdir(&self, path: &VPath) -> std::io::Result<()> {
            self.up()?;
            self.inner.mkdir(path).await
        }
        async fn rename(&self, from: &VPath, to: &VPath, overwrite: bool) -> std::io::Result<()> {
            self.up()?;
            self.inner.rename(from, to, overwrite).await
        }
        async fn remove(&self, path: &VPath, recursive: bool) -> std::io::Result<()> {
            self.up()?;
            self.inner.remove(path, recursive).await
        }
        async fn open_read(&self, path: &VPath, offset: u64) -> std::io::Result<crate::vfs::VRead> {
            self.up()?;
            if self.reads_before_drop.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.connected.send_replace(false);
                self.up()?;
            }
            if self.stall_reads.load(Ordering::SeqCst) {
                return Ok(Box::pin(Stalled));
            }
            self.inner.open_read(path, offset).await
        }
        async fn open_write(
            &self,
            path: &VPath,
            mode: crate::vfs::WriteMode,
        ) -> std::io::Result<crate::vfs::VWrite> {
            self.up()?;
            self.inner.open_write(path, mode).await
        }
        async fn set_times(
            &self,
            path: &VPath,
            modified: std::time::SystemTime,
            accessed: Option<std::time::SystemTime>,
        ) -> std::io::Result<()> {
            self.up()?;
            self.inner.set_times(path, modified, accessed).await
        }
        async fn set_mode(&self, path: &VPath, mode: u32) -> std::io::Result<()> {
            self.up()?;
            self.inner.set_mode(path, mode).await
        }
        async fn read_link(&self, path: &VPath) -> std::io::Result<VPath> {
            self.inner.read_link(path).await
        }
        async fn symlink(&self, target: &VPath, link: &VPath, dir: bool) -> std::io::Result<()> {
            self.inner.symlink(target, link, dir).await
        }
        fn is_connected(&self) -> bool {
            *self.connected.borrow()
        }
        fn connection(&self) -> Option<watch::Receiver<bool>> {
            Some(self.connected.subscribe())
        }
    }

    #[tokio::test]
    async fn a_dropped_connection_pauses_the_job_and_it_carries_on_after_a_reconnect() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        for name in ["a.txt", "b.txt", "c.txt"] {
            remote.add_file(&format!("/src/{name}"), name.as_bytes());
        }
        // The connection drops when the second file is opened.
        let flaky = Flaky::new(remote.clone(), 2);
        let local = Arc::new(MemFs::new());
        local.add_dir("/dst");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let request = TransferRequest {
            kind: TransferKind::Copy,
            sources: vec![location("remote", "/src")],
            destination: location("m", "/dst"),
            preserve_mode: true,
        };
        let view = manager
            .start(request, flaky.clone(), local.clone(), sink)
            .unwrap();

        // Paused, not failed.
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("an event")
                .expect("the job is running");
            let paused = matches!(&event, TransferEvent::Paused { fs, .. } if fs.0 == "remote");
            seen.push(event);
            if paused {
                break;
            }
        }
        assert!(!seen
            .iter()
            .any(|e| matches!(e, TransferEvent::Failed { .. })));
        assert_eq!(manager.list()[0].state, JobState::Paused);
        assert_eq!(local.read("/dst/src/a.txt").unwrap(), b"a.txt");

        flaky.reconnect();
        let rest = until_finished(&mut events).await;
        assert!(
            matches!(rest.last(), Some(TransferEvent::Finished { .. })),
            "{rest:?}"
        );
        assert!(
            !rest
                .iter()
                .any(|e| matches!(e, TransferEvent::Conflict { .. })),
            "nothing asked about the file copied before the drop"
        );
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                local.read(&format!("/dst/src/{name}")).unwrap(),
                name.as_bytes()
            );
        }
        // a.txt was not copied again: three files, three reads in all.
        assert_eq!(remote.reads(), 3);
        assert_eq!(manager.list()[0].id, view.id);
        assert_eq!(manager.list()[0].state, JobState::Done);
    }

    #[tokio::test]
    async fn a_job_queued_while_disconnected_waits_and_can_be_cancelled() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        remote.add_file("/src/a.txt", b"a");
        let flaky = Flaky::new(remote, u64::MAX);
        flaky.connected.send_replace(false);
        let local = Arc::new(MemFs::new());
        local.add_dir("/dst");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let request = TransferRequest {
            kind: TransferKind::Copy,
            sources: vec![location("remote", "/src/a.txt")],
            destination: location("m", "/dst"),
            preserve_mode: true,
        };
        let view = manager.start(request, flaky, local.clone(), sink).unwrap();
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event, TransferEvent::Paused { .. }) {
                break;
            }
            assert!(
                !matches!(event, TransferEvent::Planning { .. }),
                "nothing runs while down"
            );
        }
        assert!(manager.cancel(&view.id));
        let rest = until_finished(&mut events).await;
        assert!(matches!(rest.last(), Some(TransferEvent::Cancelled { .. })));
        assert!(!local.exists("/dst/a.txt"));
    }

    async fn until_paused(events: &mut mpsc::UnboundedReceiver<TransferEvent>) {
        loop {
            match next(events).await {
                TransferEvent::Paused { .. } => return,
                event @ (TransferEvent::Finished { .. }
                | TransferEvent::Failed { .. }
                | TransferEvent::Cancelled { .. }) => panic!("ended instead: {event:?}"),
                _ => {}
            }
        }
    }

    fn remote_request(source: &str, destination: &str) -> TransferRequest {
        TransferRequest {
            kind: TransferKind::Copy,
            sources: vec![location("remote", source)],
            destination: location("m", destination),
            preserve_mode: true,
        }
    }

    #[tokio::test]
    async fn after_a_drop_a_partial_copy_the_job_did_not_write_is_asked_about() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        remote.add_file("/src/a.txt", b"a");
        let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        remote.add_file("/src/big.bin", &big);
        // The connection drops at the first file, before big.bin.
        let flaky = Flaky::new(remote, 1);
        let local = Arc::new(MemFs::new());
        local.add_dir("/dst");
        local.add_dir("/dst/src");
        // Left by an earlier transfer, of another version of the file.
        local.add_file("/dst/src/big.bin.filepart", &[0xEE; 1024 * 1024]);
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let job = manager
            .start(
                remote_request("/src", "/dst"),
                flaky.clone(),
                local.clone(),
                sink,
            )
            .unwrap();
        until_paused(&mut events).await;
        flaky.reconnect();

        let conflict_id = loop {
            match next(&mut events).await {
                TransferEvent::Conflict {
                    conflict_id,
                    conflict,
                    ..
                } => {
                    assert!(conflict.partial, "{conflict:?}");
                    break conflict_id;
                }
                event @ TransferEvent::Finished { .. } => {
                    panic!("resumed without asking: {event:?}")
                }
                _ => {}
            }
        };
        let start_over = ConflictAnswer {
            resolution: Resolution::Overwrite,
            apply_to_all: false,
        };
        manager.resolve(&job.id, conflict_id, start_over).unwrap();
        assert!(matches!(
            until_finished(&mut events).await.last(),
            Some(TransferEvent::Finished { .. })
        ));
        assert_eq!(local.read("/dst/src/big.bin").unwrap(), big);
    }

    #[tokio::test]
    async fn a_retry_does_not_ask_again_about_answered_clashes() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        remote.add_file("/src/a.txt", b"new a");
        remote.add_file("/src/b.txt", b"b");
        // Drops when b.txt is opened, after a.txt's clash was answered.
        let flaky = Flaky::new(remote, 2);
        let local = Arc::new(MemFs::new());
        local.add_dir("/dst");
        local.add_dir("/dst/src");
        local.add_file("/dst/src/a.txt", b"old a");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let job = manager
            .start(
                remote_request("/src", "/dst"),
                flaky.clone(),
                local.clone(),
                sink,
            )
            .unwrap();
        let conflict_id = loop {
            if let TransferEvent::Conflict { conflict_id, .. } = next(&mut events).await {
                break conflict_id;
            }
        };
        let keep_both = ConflictAnswer {
            resolution: Resolution::Rename { name: None },
            apply_to_all: false,
        };
        manager.resolve(&job.id, conflict_id, keep_both).unwrap();
        until_paused(&mut events).await;
        flaky.reconnect();

        let rest = until_finished(&mut events).await;
        assert!(
            matches!(rest.last(), Some(TransferEvent::Finished { .. })),
            "{rest:?}"
        );
        assert!(
            !rest
                .iter()
                .any(|e| matches!(e, TransferEvent::Conflict { .. })),
            "asked again: {rest:?}"
        );
        assert_eq!(local.read("/dst/src/a.txt").unwrap(), b"old a");
        assert_eq!(local.read("/dst/src/a (copy).txt").unwrap(), b"new a");
        assert_eq!(local.read("/dst/src/b.txt").unwrap(), b"b");
        assert!(!local.exists("/dst/src/a (copy 2).txt"));
    }

    #[tokio::test]
    async fn paused_jobs_let_other_jobs_run() {
        let local = Arc::new(MemFs::new());
        local.add_dir("/dst");
        local.add_dir("/src");
        local.add_file("/src/here.txt", b"here");
        let manager = TransferManager::default();
        // As many paused jobs as there are slots, each on a server that's down.
        let mut paused = Vec::new();
        for server in ["ra", "rb"] {
            let remote = Arc::new(MemFs::new());
            remote.add_dir("/src");
            remote.add_file("/src/a.txt", b"a");
            let flaky = Flaky::new(remote, u64::MAX);
            flaky.connected.send_replace(false);
            let (sink, mut events) = collector();
            let request = TransferRequest {
                kind: TransferKind::Copy,
                sources: vec![location(server, "/src/a.txt")],
                destination: location("local", "/dst"),
                preserve_mode: true,
            };
            manager.start(request, flaky, local.clone(), sink).unwrap();
            until_paused(&mut events).await;
            paused.push(events);
        }

        let (sink, mut events) = collector();
        let request = TransferRequest {
            kind: TransferKind::Copy,
            sources: vec![location("local", "/src/here.txt")],
            destination: location("local", "/dst"),
            preserve_mode: true,
        };
        manager
            .start(request, local.clone(), local.clone(), sink)
            .unwrap();
        assert!(matches!(
            until_finished(&mut events).await.last(),
            Some(TransferEvent::Finished { .. })
        ));
        assert_eq!(local.read("/dst/here.txt").unwrap(), b"here");
    }

    #[tokio::test]
    async fn cancel_stops_a_job_whose_server_stalls() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        remote.add_file("/src/a.txt", b"a");
        let flaky = Flaky::new(remote, u64::MAX);
        flaky.stall_reads.store(true, Ordering::SeqCst);
        let local = Arc::new(MemFs::new());
        local.add_dir("/dst");
        let manager = TransferManager::default();
        let (sink, mut events) = collector();
        let job = manager
            .start(
                remote_request("/src/a.txt", "/dst"),
                flaky,
                local.clone(),
                sink,
            )
            .unwrap();
        while !matches!(next(&mut events).await, TransferEvent::Planned { .. }) {}
        tokio::time::sleep(Duration::from_millis(100)).await;

        let asked = Instant::now();
        assert!(manager.cancel(&job.id));
        assert!(matches!(
            until_finished(&mut events).await.last(),
            Some(TransferEvent::Cancelled { .. })
        ));
        assert!(asked.elapsed() < CANCEL_GRACE, "{:?}", asked.elapsed());
        // Its temporary file is gone too.
        assert_eq!(
            local
                .paths()
                .iter()
                .filter(|p| p.contains("corepty"))
                .count(),
            0
        );
    }
}
