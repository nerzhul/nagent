//! Sticky per-session dispatch tests for [`WorkerPool`].
//!
//! These live in `tests/` rather than inside the `worker.rs` `mod tests`
//! so they exercise the pool through its **public** API only — the same
//! surface the server uses — and so they can be run with
//! `cargo test -p stt-core --test pool_round_robin` independently of
//! the rest of the unit suite.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use stt_core::backend::{BackendError, BackendInfo, WhisperBackend};
use stt_core::job::{InferRequest, InferResponse, InferenceJob};
use stt_core::{InferenceWorker, PoolDispatch, WorkerPool};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Counting backend: every call increments `seen` and sleeps a fixed
/// amount so the test can assert parallelism / serialization.
#[derive(Debug, Clone)]
struct CountingBackend {
    seen: Arc<AtomicUsize>,
    delay: Duration,
}

#[async_trait]
impl WhisperBackend for CountingBackend {
    async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError> {
        // Sleep on a blocking-pool task so the async runtime stays
        // responsive and other workers can actually run in parallel.
        let delay = self.delay;
        tokio::task::spawn_blocking(move || std::thread::sleep(delay))
            .await
            .unwrap();
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
        "counting"
    }

    fn model_id(&self) -> &str {
        "mock-count"
    }

    fn info(&self) -> BackendInfo {
        BackendInfo::minimal("counting", "mock-count")
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

/// Same session, 2 workers: the second job must wait for the first, so
/// total wall time is roughly 2 × `delay`, NOT `delay`. This is the
/// per-session FIFO / sticky-dispatch guarantee.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sticky_dispatch_serializes_same_session_across_workers() {
    let seen = Arc::new(AtomicUsize::new(0));
    let delay = Duration::from_millis(200);
    let pool = WorkerPool::spawn(2, {
        let seen = Arc::clone(&seen);
        move || -> Arc<dyn WhisperBackend> {
            Arc::new(CountingBackend {
                seen: Arc::clone(&seen),
                delay,
            })
        }
    });

    let session = Uuid::new_v4();
    let (job_a, rx_a) = make_job(session);
    let (job_b, rx_b) = make_job(session);

    let (first_elapsed, total) = {
        let dispatch = pool.dispatch();
        dispatch.send(job_a).await.unwrap();
        dispatch.send(job_b).await.unwrap();

        let start = Instant::now();
        let resp_a = rx_a.await.unwrap();
        let first_elapsed = start.elapsed();
        let resp_b = rx_b.await.unwrap();
        let total = start.elapsed();

        assert_eq!(resp_a.session_id, session);
        assert_eq!(resp_b.session_id, session);
        (first_elapsed, total)
    };

    assert_eq!(seen.load(Ordering::SeqCst), 2);

    // Both jobs are forced through the *same* worker because they share
    // a session_id, so they run sequentially. With a 200 ms delay each,
    // the total must be at least ~400 ms (no parallelism within a
    // session) but not absurdly longer (allow a generous slack for CI
    // runners under load).
    assert!(
        total >= delay * 2,
        "expected at least 2 * delay = {:?}, got {total:?}",
        delay * 2,
    );
    assert!(total < delay * 5, "jobs took suspiciously long: {total:?}");
    // The first response should land in roughly `delay` time, the
    // second in another `delay`. We sanity-check that ordering is
    // respected at the wall-clock level.
    assert!(
        first_elapsed < total,
        "first response must arrive before second ({first_elapsed:?} >= {total:?})"
    );

    // Drop the dispatch clone so the worker's mpsc closes when
    // `shutdown` drops its own clones; otherwise the worker hangs on
    // `rx.recv().await` forever and the join blocks.
    pool.shutdown().await.unwrap();
}

/// Different sessions, 2 workers: the two jobs *can* run in parallel.
/// Total wall time should be roughly `delay`, not `2 * delay`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_sessions_can_run_in_parallel() {
    let seen = Arc::new(AtomicUsize::new(0));
    let delay = Duration::from_millis(200);
    let pool = WorkerPool::spawn(2, {
        let seen = Arc::clone(&seen);
        move || -> Arc<dyn WhisperBackend> {
            Arc::new(CountingBackend {
                seen: Arc::clone(&seen),
                delay,
            })
        }
    });

    // Pick sessions that hash to different shards so they really do
    // land on different workers. (Random UUIDs almost always do, but
    // try a few until we get a spread.) `dispatch` is borrowed by the
    // shard-selection loop, then moved into the test body so it drops
    // at the end of the block and the worker channels can close.
    let (s_a, s_b, job_a, rx_a, job_b, rx_b) = {
        let dispatch = pool.dispatch();
        let (a, b) = loop {
            let a = Uuid::new_v4();
            let b = Uuid::new_v4();
            if dispatch.shard_for(a) != dispatch.shard_for(b) {
                break (a, b);
            }
        };
        let (job_a, rx_a) = make_job(a);
        let (job_b, rx_b) = make_job(b);
        (a, b, job_a, rx_a, job_b, rx_b)
    };
    let _ = s_a;
    let _ = s_b;

    let total = {
        let dispatch = pool.dispatch();
        dispatch.send(job_a).await.unwrap();
        dispatch.send(job_b).await.unwrap();

        let start = Instant::now();
        let _ = rx_a.await.unwrap();
        let _ = rx_b.await.unwrap();
        start.elapsed()
    };

    assert_eq!(seen.load(Ordering::SeqCst), 2);
    // Parallel execution should fit well within 2 × delay. Allow a
    // generous slack for the worst-case CI runner.
    assert!(
        total < delay * 2,
        "two sessions ran serially, not in parallel: {total:?}"
    );
    // And obviously they must still take at least one delay.
    assert!(total >= delay, "jobs returned suspiciously fast: {total:?}");

    pool.shutdown().await.unwrap();
}

/// Pool sized to 1 still works as a thin wrapper around
/// [`InferenceWorker`] — single-worker deployments keep working.
#[tokio::test]
async fn single_worker_pool_still_serializes() {
    let seen = Arc::new(AtomicUsize::new(0));
    let delay = Duration::from_millis(50);
    let pool = WorkerPool::spawn(1, {
        let seen = Arc::clone(&seen);
        move || -> Arc<dyn WhisperBackend> {
            Arc::new(CountingBackend {
                seen: Arc::clone(&seen),
                delay,
            })
        }
    });
    assert_eq!(pool.worker_count(), 1);

    let s = Uuid::new_v4();
    let (job_a, rx_a) = make_job(s);
    let (job_b, rx_b) = make_job(s);

    let (resp_a, resp_b) = {
        let dispatch = pool.dispatch();
        assert_eq!(dispatch.worker_count(), 1);
        dispatch.send(job_a).await.unwrap();
        dispatch.send(job_b).await.unwrap();
        let resp_a = rx_a.await.unwrap();
        let resp_b = rx_b.await.unwrap();
        (resp_a, resp_b)
    };
    assert_eq!(resp_a.session_id, s);
    assert_eq!(resp_b.session_id, s);
    assert_eq!(seen.load(Ordering::SeqCst), 2);

    pool.shutdown().await.unwrap();
}

/// Dropping every [`PoolDispatch`] and calling `shutdown` must release
/// the workers cleanly — no hang, no panic on join.
#[tokio::test]
async fn pool_shutdown_drains_workers() {
    let pool = WorkerPool::spawn(3, || -> Arc<dyn WhisperBackend> {
        Arc::new(CountingBackend {
            seen: Arc::new(AtomicUsize::new(0)),
            delay: Duration::from_millis(10),
        })
    });
    {
        let _dispatch: PoolDispatch = pool.dispatch();
        // dispatch drops at end of scope, releasing its clones of the
        // worker `mpsc::Sender`s; `pool.shutdown` then drops its own
        // clone and the channel finally closes.
    }
    // Bound the shutdown — CI runners sometimes stall forever on
    // background tasks, and that should fail the test loudly.
    tokio::time::timeout(Duration::from_secs(2), pool.shutdown())
        .await
        .expect("pool did not shut down in time")
        .expect("worker join failed");
}

/// The pool must continue serving jobs after the early ones complete —
/// shutdown is not "one batch and out".
#[tokio::test]
async fn pool_processes_multiple_waves() {
    let seen = Arc::new(AtomicUsize::new(0));
    let pool = WorkerPool::spawn(2, {
        let seen = Arc::clone(&seen);
        move || -> Arc<dyn WhisperBackend> {
            Arc::new(CountingBackend {
                seen: Arc::clone(&seen),
                delay: Duration::from_millis(10),
            })
        }
    });

    {
        let dispatch = pool.dispatch();
        for wave in 0..3 {
            let s = Uuid::new_v4();
            let (job, rx) = make_job(s);
            dispatch.send(job).await.unwrap();
            let resp = rx.await.unwrap();
            assert_eq!(resp.session_id, s, "wave {wave}");
        }
    }
    assert_eq!(seen.load(Ordering::SeqCst), 3);

    pool.shutdown().await.unwrap();
}

/// [`stt_core::shard_for`] is exposed as a free function so the routing
/// math is testable in isolation. Assert the basic properties.
#[test]
fn shard_for_is_in_range_and_stable() {
    let s = Uuid::nil();
    for n in 1..16 {
        let idx = stt_core::shard_for(s, n);
        assert!(idx < n, "{idx} not < {n}");
    }
    let s = Uuid::new_v4();
    assert_eq!(stt_core::shard_for(s, 4), stt_core::shard_for(s, 4));
}

/// The [`InferenceWorker`] (single-worker) still exists for
/// backwards-compat / small deployments. This test pins that it can be
/// spawned with a fresh `mpsc::channel` the same way the server does.
#[tokio::test]
async fn single_worker_back_compat_still_works() {
    let seen = Arc::new(AtomicUsize::new(0));
    let backend: Arc<dyn WhisperBackend> = Arc::new(CountingBackend {
        seen: Arc::clone(&seen),
        delay: Duration::from_millis(1),
    });
    let (tx, rx) = mpsc::channel::<InferenceJob>(4);
    let handle = InferenceWorker::spawn(backend, rx);

    let s = Uuid::new_v4();
    let (job, resp_rx) = make_job(s);
    tx.send(job).await.unwrap();
    drop(tx);

    let resp = resp_rx.await.unwrap();
    assert_eq!(resp.session_id, s);
    handle.join().await.unwrap();
}
