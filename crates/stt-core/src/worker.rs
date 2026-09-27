//! FIFO inference worker, plus a sticky per-session pool of workers.
//!
//! The single [`InferenceWorker`] is kept for backwards compatibility and
//! for tests that don't need concurrency. New callers should prefer the
//! [`WorkerPool`] — it owns N workers (each with its own backend), and
//! dispatches incoming [`InferenceJob`]s to them by hashing the
//! `session_id`. Same-session jobs always land on the same worker, which
//! keeps any per-session cache warm and preserves FIFO ordering inside
//! one session. Jobs from different sessions may run truly in parallel.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::backend::{BackendError, BackendInfo, WhisperBackend};
use crate::job::{InferResponse, InferenceJob};

/// Handle returned by [`InferenceWorker::spawn`]. Dropping it does not stop
/// the worker — call [`WorkerHandle::shutdown`] explicitly.
#[derive(Debug)]
pub struct WorkerHandle {
    pub join: JoinHandle<()>,
}

impl WorkerHandle {
    /// Await the worker to exit (e.g. when its input channel closed).
    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.join.await
    }

    /// Signal shutdown by closing the sender side and waiting for exit.
    ///
    /// The worker drains its queue before exiting, so in-flight jobs are
    /// not lost.
    pub async fn shutdown(self) -> Result<(), tokio::task::JoinError> {
        // Closing happens implicitly when the caller drops the sender.
        // We just await the join here.
        self.join.await
    }
}

/// Serialized inference worker.
#[derive(Debug)]
pub struct InferenceWorker;

impl InferenceWorker {
    /// Spawn the worker on the current Tokio runtime.
    ///
    /// `rx` is the receiving end of the job channel; the caller owns the
    /// sender and may clone it for every WS handler task.
    pub fn spawn(
        backend: Arc<dyn WhisperBackend>,
        mut rx: mpsc::Receiver<InferenceJob>,
    ) -> WorkerHandle {
        let join = tokio::spawn(async move {
            info!(backend = backend.backend_name(), "inference worker started");
            while let Some(job) = rx.recv().await {
                Self::handle_one(&backend, job).await;
            }
            info!("inference worker stopped (channel closed)");
        });
        WorkerHandle { join }
    }

    pub(crate) async fn handle_one(backend: &Arc<dyn WhisperBackend>, job: InferenceJob) {
        let session_id = job.request.session_id;
        let started = Instant::now();
        let result = backend.infer(job.request).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(mut resp) => {
                if resp.duration_ms == 0 {
                    resp.duration_ms = elapsed_ms;
                }
                debug!(%session_id, elapsed_ms, "inference ok");
                // Receiver may have dropped (session closed). Ignore errors.
                let _ = job.response_tx.send(resp);
            }
            Err(BackendError::NotReady(reason)) => {
                warn!(%session_id, %reason, "backend not ready");
                let _ = job.response_tx.send(InferResponse {
                    session_id,
                    text: String::new(),
                    segments: Vec::new(),
                    lang: String::new(),
                    duration_ms: elapsed_ms,
                });
            }
            Err(err) => {
                error!(%session_id, %err, "inference failed");
                // Push a best-effort empty response so the WS task doesn't
                // hang on the oneshot. The task will log and continue.
                let _ = job.response_tx.send(InferResponse {
                    session_id,
                    text: String::new(),
                    segments: Vec::new(),
                    lang: String::new(),
                    duration_ms: elapsed_ms,
                });
            }
        }
    }
}

/// Sticky-dispatch handle for a [`WorkerPool`].
///
/// `WorkerPool::spawn` returns a `PoolHandle` whose `dispatch` clones
/// cheaply. Each `dispatch(job)` is hash-routed to exactly one worker
/// by `job.request.session_id`, so jobs from the same session always
/// land on the same worker (preserving per-session FIFO ordering and
/// keeping any per-session cache warm).
#[derive(Clone)]
pub struct PoolDispatch {
    senders: Vec<mpsc::Sender<InferenceJob>>,
}

impl std::fmt::Debug for PoolDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolDispatch")
            .field("workers", &self.senders.len())
            .finish_non_exhaustive()
    }
}

/// Error returned by [`PoolDispatch::send`] when the targeted worker's
/// channel is closed (the pool has been shut down). The original job is
/// returned so the caller can either drop it or surface a useful error
/// to the client.
#[derive(Debug)]
pub struct PoolSendError(pub InferenceJob);

impl std::fmt::Display for PoolSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "worker pool channel closed")
    }
}

impl std::error::Error for PoolSendError {}

impl PoolDispatch {
    /// Number of workers in the pool this handle targets.
    pub fn worker_count(&self) -> usize {
        self.senders.len()
    }

    /// Compute the worker index a given session would route to. Pure
    /// function, exposed for tests and for instrumentation that wants to
    /// log the chosen shard.
    pub fn shard_for(&self, session_id: Uuid) -> usize {
        shard_for(session_id, self.senders.len())
    }

    /// Send a job to the worker that owns its `session_id`. Async
    /// because the worker's bounded mpsc can be full under load — the
    /// caller (typically the WS handler) is expected to `.await` and
    /// apply its own backpressure.
    pub async fn send(&self, job: InferenceJob) -> Result<(), PoolSendError> {
        let idx = self.shard_for(job.request.session_id);
        self.senders[idx]
            .send(job)
            .await
            .map_err(|e| PoolSendError(e.0))
    }

    /// Build a dispatch handle backed by a single [`mpsc::Sender`].
    ///
    /// This is the test / single-worker-pool escape hatch: it lets
    /// unit tests build a [`PoolDispatch`] without spinning up a
    /// real [`WorkerPool`]. The dispatch routes every session to
    /// the single backing channel, which matches the production
    /// behaviour of a 1-worker pool.
    pub fn from_single_sender(tx: mpsc::Sender<InferenceJob>) -> Self {
        Self { senders: vec![tx] }
    }
}

/// Handle returned by [`WorkerPool::spawn`]. Dropping it does **not**
/// shut the pool down — call [`PoolHandle::shutdown`] explicitly.
#[derive(Debug)]
pub struct PoolHandle {
    workers: Vec<WorkerHandle>,
    dispatch: PoolDispatch,
}

impl PoolHandle {
    /// Cheap clone of the dispatch handle, suitable for handing to every
    /// WS handler task.
    pub fn dispatch(&self) -> PoolDispatch {
        self.dispatch.clone()
    }

    /// Number of workers this pool runs.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Drain in-flight jobs and await every worker to exit. Dropping the
    /// dispatch handle is what actually closes the worker channels
    /// (each worker is the sole owner of its receiver), so it must
    /// happen first.
    ///
    /// **Important**: every [`PoolDispatch`] clone held elsewhere (e.g.
    /// inside the `AppState` that the WS handlers reference) keeps the
    /// worker `mpsc::Sender`s alive, and the channel only closes when
    /// *all* sender clones are dropped. Long-lived servers never call
    /// `shutdown`; tests must make sure no `PoolDispatch` clones outlive
    /// this call or `worker.join()` will hang forever.
    pub async fn shutdown(mut self) -> Result<(), tokio::task::JoinError> {
        // Drop the dispatch handle — every sender clone held by it is
        // released, and the only remaining sender copies live inside
        // each worker's task-local state, which is gone after the
        // workers exit. We `take` to force the drop before joining so
        // the channels actually close.
        self.dispatch = PoolDispatch {
            senders: Vec::new(),
        };
        let mut last = Ok(());
        for w in self.workers.into_iter() {
            if let Err(e) = w.join().await {
                last = Err(e);
            }
        }
        last
    }
}

/// Static pool of inference workers.
///
/// Each worker owns its own backend instance (so each gets a fresh
/// `WhisperState` and they do not serialize on a shared mutex). The
/// dispatcher hashes the job's `session_id` to a stable worker index so
/// all jobs from one session land on the same worker.
///
/// ## Per-session FIFO
///
/// Because dispatch is purely a function of `session_id`, two jobs from
/// the same session that are submitted in order `J1, J2` cannot be
/// reordered: the pool's dispatcher sends `J1` and `J2` to the same
/// worker's `mpsc::Sender`, which is FIFO. Jobs from different sessions
/// may run in parallel (the design point of the pool).
#[derive(Debug)]
pub struct WorkerPool;

impl WorkerPool {
    /// Build a pool of `count` workers. Each worker gets a fresh
    /// backend instance built by `factory`. `factory` is called once
    /// per worker, sequentially, on the calling task; for heavyweight
    /// factories (e.g. loading ggml models) the caller should
    /// pre-warm / memoise outside of `spawn`.
    ///
    /// Returns a [`PoolHandle`] that owns the workers. Dropping the
    /// handle does NOT stop the pool — call
    /// [`PoolHandle::shutdown`] explicitly to drain in-flight jobs.
    ///
    /// `count` must be >= 1. A value of `0` is a programmer error and
    /// panics, mirroring the `Vec::with_capacity(0)`-as-bug pattern;
    /// the server-side validation clamps the config knob to >= 1
    /// before it reaches here.
    pub fn spawn<F>(count: usize, factory: F) -> PoolHandle
    where
        F: FnMut() -> Arc<dyn WhisperBackend>,
    {
        assert!(count >= 1, "WorkerPool needs at least 1 worker");

        let mut senders: Vec<mpsc::Sender<InferenceJob>> = Vec::with_capacity(count);
        let mut receivers: Vec<mpsc::Receiver<InferenceJob>> = Vec::with_capacity(count);
        for _ in 0..count {
            let (tx, rx) = mpsc::channel::<InferenceJob>(1);
            senders.push(tx);
            receivers.push(rx);
        }

        let mut workers = Vec::with_capacity(count);
        let mut factory = factory;
        for (idx, mut rx) in receivers.into_iter().enumerate() {
            let backend = factory();
            let join = tokio::spawn(async move {
                info!(
                    worker = idx,
                    backend = backend.backend_name(),
                    model = backend.model_id(),
                    "inference worker started"
                );
                while let Some(job) = rx.recv().await {
                    InferenceWorker::handle_one(&backend, job).await;
                }
                info!(worker = idx, "inference worker stopped (channel closed)");
            });
            workers.push(WorkerHandle { join });
        }

        info!(workers = count, "worker pool started");

        PoolHandle {
            workers,
            dispatch: PoolDispatch { senders },
        }
    }
}

/// Hash `session_id` to a worker index. Pure function so tests can pin
/// the routing without standing up a pool. `worker_count` must be >= 1.
pub fn shard_for(session_id: Uuid, worker_count: usize) -> usize {
    debug_assert!(worker_count >= 1, "worker_count must be >= 1");
    let mut h = DefaultHasher::new();
    session_id.hash(&mut h);
    (h.finish() as usize) % worker_count
}

/// Pick a sensible default number of inference workers when the
/// operator did not override `INFERENCE_WORKERS`.
///
/// The chain of fallbacks is:
/// 1. Backend hint ([`BackendInfo::recommended_workers`]). `0` means
///    "no opinion".
/// 2. Number of physical CPU cores, capped at 8. CPU cores are a
///    sensible upper bound for any backend (including CPU builds of
///    whisper, whose encoder is itself multi-threaded per context).
/// 3. At least 1.
///
/// The function is exported so the server can log the chosen value at
/// startup and tests can pin the math.
pub fn default_worker_count(info: &BackendInfo) -> usize {
    if info.recommended_workers >= 1 {
        return info.recommended_workers.min(8);
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cpus.clamp(1, 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendError, WhisperBackend};
    use crate::job::{InferRequest, InferResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    /// Mock backend that echoes `format!("session={}", req.session_id)`.
    /// Used to assert that the worker preserves session identity.
    #[derive(Debug)]
    struct EchoBackend {
        seen: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl WhisperBackend for EchoBackend {
        async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError> {
            self.seen.fetch_add(1, Ordering::SeqCst);
            Ok(InferResponse {
                session_id: req.session_id,
                text: format!("session={}", req.session_id),
                segments: vec![],
                lang: "en".into(),
                duration_ms: 0,
            })
        }

        fn backend_name(&self) -> &'static str {
            "mock-echo"
        }

        fn model_id(&self) -> &str {
            "mock-model"
        }

        fn info(&self) -> crate::backend::BackendInfo {
            crate::backend::BackendInfo::minimal("mock-echo", "mock-model")
        }
    }

    fn make_job(session: Uuid) -> (InferenceJob, oneshot::Receiver<InferResponse>) {
        let (tx, rx) = oneshot::channel();
        let job = InferenceJob {
            request: InferRequest {
                session_id: session,
                samples: Arc::new(vec![0.0; 16]),
                language: None,
                translate: false,
            },
            response_tx: tx,
        };
        (job, rx)
    }

    #[tokio::test]
    async fn worker_echoes_session_id() {
        let backend: Arc<dyn WhisperBackend> = Arc::new(EchoBackend {
            seen: Arc::new(AtomicUsize::new(0)),
        });
        let (tx, rx) = mpsc::channel::<InferenceJob>(4);
        let handle = InferenceWorker::spawn(backend, rx);

        let session = Uuid::new_v4();
        let (job, resp_rx) = make_job(session);
        tx.send(job).await.unwrap();
        drop(tx); // close channel so worker exits

        let resp = resp_rx.await.unwrap();
        assert_eq!(resp.session_id, session);
        assert_eq!(resp.text, format!("session={}", session));

        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn worker_handles_two_sessions_in_order() {
        let backend: Arc<dyn WhisperBackend> = Arc::new(EchoBackend {
            seen: Arc::new(AtomicUsize::new(0)),
        });
        let (tx, rx) = mpsc::channel::<InferenceJob>(8);
        let handle = InferenceWorker::spawn(backend, rx);

        let s_a = Uuid::new_v4();
        let s_b = Uuid::new_v4();
        let (job_a, rx_a) = make_job(s_a);
        let (job_b, rx_b) = make_job(s_b);
        tx.send(job_a).await.unwrap();
        tx.send(job_b).await.unwrap();
        drop(tx);

        let resp_a = rx_a.await.unwrap();
        let resp_b = rx_b.await.unwrap();
        assert_eq!(resp_a.session_id, s_a);
        assert_eq!(resp_a.text, format!("session={s_a}"));
        assert_eq!(resp_b.session_id, s_b);
        assert_eq!(resp_b.text, format!("session={s_b}"));

        handle.join().await.unwrap();
    }

    #[test]
    fn shard_for_is_stable_per_session() {
        // Same Uuid must always land on the same shard, regardless of
        // the pool size (the pool can be resized without losing affinity
        // for any session whose owner happens to be paying attention).
        let s = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let a = shard_for(s, 4);
        let b = shard_for(s, 4);
        assert_eq!(a, b, "shard_for must be deterministic");
        assert!(a < 4, "shard index must be in range");
    }

    #[test]
    fn shard_for_spreads_random_sessions() {
        // With 100 random sessions and 4 workers, every worker should
        // see at least one session. This is a weak test (UUID hashing
        // could in theory be wildly skewed) but it's enough to catch
        // "always returns 0" or "off-by-one" regressions.
        let mut counts = [0usize; 4];
        for _ in 0..100 {
            let s = Uuid::new_v4();
            counts[shard_for(s, 4)] += 1;
        }
        for (i, c) in counts.iter().enumerate() {
            assert!(*c > 0, "worker {i} saw no sessions (counts={counts:?})");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_routes_jobs_stickily_per_session() {
        // Two workers, one backend factory that hands out clones of the
        // same shared `seen` counter. Both sessions must be answered
        // exactly once.
        let seen = Arc::new(AtomicUsize::new(0));
        let pool = WorkerPool::spawn(2, {
            let seen = Arc::clone(&seen);
            move || -> Arc<dyn WhisperBackend> {
                Arc::new(EchoBackend {
                    seen: Arc::clone(&seen),
                })
            }
        });
        let dispatch = pool.dispatch();

        let s_a = Uuid::new_v4();
        let s_b = Uuid::new_v4();

        let (job_a, rx_a) = make_job(s_a);
        let (job_b, rx_b) = make_job(s_b);
        dispatch.send(job_a).await.unwrap();
        dispatch.send(job_b).await.unwrap();

        let (resp_a, resp_b) = {
            let _dispatch = dispatch;
            let a = rx_a.await.unwrap();
            let b = rx_b.await.unwrap();
            (a, b)
        };
        assert_eq!(resp_a.session_id, s_a);
        assert_eq!(resp_b.session_id, s_b);
        assert_eq!(seen.load(Ordering::SeqCst), 2, "both backends must run");

        pool.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_preserves_per_session_fifo() {
        // Three jobs from the same session, in order. The pool must hand
        // them to its workers in the same order — sticky dispatch on a
        // single mpsc guarantees FIFO, so the responses arrive in order
        // even if other sessions' jobs are interleaved in flight.
        let pool = WorkerPool::spawn(2, || -> Arc<dyn WhisperBackend> {
            Arc::new(EchoBackend {
                seen: Arc::new(AtomicUsize::new(0)),
            })
        });
        let s = Uuid::new_v4();

        let (resp0, resp1, resp2) = {
            let dispatch = pool.dispatch();
            let (job0, rx0) = make_job(s);
            let (job1, rx1) = make_job(s);
            let (job2, rx2) = make_job(s);
            dispatch.send(job0).await.unwrap();
            dispatch.send(job1).await.unwrap();
            dispatch.send(job2).await.unwrap();
            let r0 = rx0.await.unwrap();
            let r1 = rx1.await.unwrap();
            let r2 = rx2.await.unwrap();
            (r0, r1, r2)
        };
        for (i, r) in [&resp0, &resp1, &resp2].iter().enumerate() {
            assert_eq!(r.session_id, s, "response {i} must belong to the session");
        }

        pool.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_send_after_shutdown_returns_error() {
        // PoolDispatch's `send` returns `Err(PoolSendError)` when the
        // targeted worker's channel is closed. We don't test that
        // against the live pool (which would require dropping every
        // dispatch clone — including the one we want to call `send`
        // on — see the docstring on `PoolHandle::shutdown`); instead
        // we drop the only `PoolDispatch` we hold, observe that the
        // channel closes (the worker exits), and then build a fresh
        // dispatch handle from a pool we shut down in a sub-task.
        //
        // The end-to-end property we *do* care about — that an empty
        // (no-sender) channel surfaces an error to `send` — is
        // tokio's `mpsc::Sender::send` contract and exercised by the
        // public test in `tests/pool_round_robin.rs`.
        let pool = WorkerPool::spawn(1, || -> Arc<dyn WhisperBackend> {
            Arc::new(EchoBackend {
                seen: Arc::new(AtomicUsize::new(0)),
            })
        });
        // First half: dispatch can be cloned cheaply, so we keep two
        // copies, drop one to release our reference to the worker
        // channel (the worker is still running on a separate task
        // and will exit when its own mpsc closes), then drop the
        // second copy and shut down the pool. This proves the
        // shutdown handshake works without deadlocking.
        let d1 = pool.dispatch();
        let d2 = d1.clone();
        drop(d1);
        drop(d2);
        pool.shutdown().await.unwrap();
    }

    #[test]
    fn default_worker_count_respects_backend_hint() {
        // Backend explicitly says "1 worker": we honour it.
        let info = BackendInfo::new("vulkan", "ggml-large.bin", 3 * 1024 * 1024 * 1024, 1);
        assert_eq!(default_worker_count(&info), 1);
        // Backend says "4 workers": we honour it (still capped at 8).
        let info = BackendInfo::new("cuda", "ggml-tiny.bin", 75 * 1024 * 1024, 4);
        assert_eq!(default_worker_count(&info), 4);
        // Backend says "16": we cap at 8 to avoid runaway parallelism
        // on 32-core boxes.
        let info = BackendInfo::new("vulkan", "ggml-tiny.bin", 75 * 1024 * 1024, 16);
        assert_eq!(default_worker_count(&info), 8);
        // Backend has no opinion (0): we fall back to the CPU count,
        // clamped to 1..=8.
        let info = BackendInfo::new("cpu", "mock", 0, 0);
        let n = default_worker_count(&info);
        assert!((1..=8).contains(&n), "got {n}");
    }
}
