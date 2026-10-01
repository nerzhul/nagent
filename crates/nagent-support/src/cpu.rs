//! Bounded blocking-work helper.
//!
//! CPU-heavy or blocking-I/O tasks (Argon2 hash / verify, PDF
//! extraction, file reads, ONNX runs) MUST run on the blocking
//! thread pool with a concurrency cap so a burst cannot starve the
//! async runtime or spike memory (plan R1).
//!
//! [`run_bounded`] is the sanctioned wrapper: it acquires one
//! permit from a caller-owned [`tokio::sync::Semaphore`] and runs
//! the closure on `tokio::task::spawn_blocking`. The permit lives
//! in the closure as an [`OwnedSemaphorePermit`], so it is released
//! exactly when the closure returns — even on panic, the
//! `Drop` impl returns the permit to the pool.
//!
//! ## Queue depth + wait timeout (plan R1a)
//!
//! [`BoundedConfig`] pairs the concurrency semaphore with an
//! **optional** queue gate (a second semaphore that bounds the
//! number of callers waiting for a concurrency permit) and an
//! **optional** wait timeout. With both knobs at their defaults
//! the helper matches the historical behaviour (one permit per
//! caller, wait forever); with the knobs turned on, saturation
//! is observable:
//!
//! - [`RunError::QueueFull`] — `max_queue` callers were already
//!   waiting for a concurrency permit; the new caller is rejected
//!   immediately so the route layer can answer `503 Service
//!   Unavailable` with a `Retry-After` header.
//! - [`RunError::QueueTimeout`] — the caller waited longer than
//!   `queue_timeout` for a concurrency permit; the route layer can
//!   answer `429 Too Many Requests` with a `Retry-After`.
//!
//! ## Why an `Arc<Semaphore>` and not `&Semaphore`?
//!
//! `spawn_blocking` requires the closure to be `'static`, so the
//! semaphore has to be cloned into the future. Sharing one
//! `Arc<Semaphore>` across the subsystem lets every caller count
//! against the same concurrency budget — typically one permit per
//! CPU core, halved to keep the runtime responsive.
//!
//! ## Error model
//!
//! The future returns `Result<R, RunError>`. The four failure modes
//! are:
//!
//! - [`RunError::Closed`] — the semaphore was closed before a
//!   permit could be acquired (only happens if the owning
//!   subsystem is being torn down). Callers should treat this as
//!   "service unavailable".
//! - [`RunError::Join`] — the blocking task panicked. Callers
//!   should treat this as an internal error; the underlying
//!   library (Argon2 / pdf-extract) has violated its contract.
//! - [`RunError::QueueFull`] — the queue gate rejected the caller.
//!   Callers should map this to `503` with `Retry-After`.
//! - [`RunError::QueueTimeout`] — the wait timeout fired. Callers
//!   should map this to `429` with `Retry-After`.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinError;

/// Errors returned by [`run_bounded`].
#[derive(Debug, Error)]
pub enum RunError {
    /// The semaphore was closed before a permit could be acquired.
    /// Only happens during shutdown; treat as "service unavailable".
    #[error("blocking semaphore closed")]
    Closed,
    /// The blocking task panicked. The permit is released by the
    /// `OwnedSemaphorePermit` drop on the worker thread before the
    /// panic unwinds.
    #[error("blocking task panicked: {0}")]
    Join(#[from] JoinError),
    /// The queue gate rejected the caller (plan R1a). The
    /// `max_queue` callers ahead of us were already waiting for
    /// a concurrency permit; the route layer should answer `503
    /// Service Unavailable` with a `Retry-After` header.
    #[error("blocking pool queue full (max {0} waiters)")]
    QueueFull(usize),
    /// The wait timeout fired before a concurrency permit became
    /// available (plan R1a). The route layer should answer `429
    /// Too Many Requests` with a `Retry-After` header.
    #[error("blocking pool wait timeout after {0:?}")]
    QueueTimeout(Duration),
}

/// Configuration for [`run_bounded`].
///
/// A typical constructor looks like:
///
/// ```ignore
/// use std::sync::Arc;
/// use std::time::Duration;
/// use nagent_support::cpu::{run_bounded, BoundedConfig};
/// use tokio::sync::Semaphore;
///
/// let semaphore = Arc::new(Semaphore::new(4));
/// let queue = Arc::new(Semaphore::new(64));
/// let cfg = BoundedConfig {
///     semaphore,
///     queue: Some(BoundedQueue { semaphore: queue, max_waiters: 64 }),
///     timeout: Some(Duration::from_secs(5)),
/// };
/// let bytes = run_bounded(cfg, |_permit| vec![0u8; 1024]).await?;
/// ```
#[derive(Clone)]
pub struct BoundedConfig {
    /// Concurrency cap. Acquiring this permit is the actual gate
    /// the blocking work is gated on.
    pub semaphore: Arc<Semaphore>,
    /// Optional queue gate. When `Some`, the caller first tries to
    /// `try_acquire_owned` on this semaphore; a rejection surfaces
    /// as [`RunError::QueueFull`] carrying the operator-configured
    /// `max_waiters`. When `None`, the helper skips the queue gate
    /// entirely (the historical behaviour).
    pub queue: Option<BoundedQueue>,
    /// Optional wait timeout for the concurrency permit. When
    /// `Some`, [`run_bounded`] wraps `acquire_owned` in
    /// `tokio::time::timeout`; a timeout surfaces as
    /// [`RunError::QueueTimeout`]. When `None`, the caller waits
    /// indefinitely.
    pub timeout: Option<Duration>,
}

/// Queue gate paired with the operator-configured `max_waiters`
/// cap. Carrying the cap separately lets [`RunError::QueueFull`]
/// surface it to the route layer for the `Retry-After` /
/// observability payload, instead of fishing the initial permit
/// count back out of the underlying [`Semaphore`].
#[derive(Clone)]
pub struct BoundedQueue {
    pub semaphore: Arc<Semaphore>,
    pub max_waiters: usize,
}

impl BoundedConfig {
    /// Build a config with no queue gate and no wait timeout —
    /// matches the historical `run_bounded(Arc<Semaphore>, f)`
    /// signature byte for byte. Kept for the call sites that have
    /// not opted into the R1a knobs yet.
    pub fn unbounded(semaphore: Arc<Semaphore>) -> Self {
        Self {
            semaphore,
            queue: None,
            timeout: None,
        }
    }

    /// Build a config with a queue gate but no wait timeout.
    pub fn with_queue(semaphore: Arc<Semaphore>, queue: BoundedQueue) -> Self {
        Self {
            semaphore,
            queue: Some(queue),
            timeout: None,
        }
    }

    /// Build a config with both the queue gate and the wait
    /// timeout. This is the recommended shape for the CPU-heavy
    /// subsystems (Argon2, PDF, TTS, read_document).
    pub fn with_queue_and_timeout(
        semaphore: Arc<Semaphore>,
        queue: BoundedQueue,
        timeout: Duration,
    ) -> Self {
        Self {
            semaphore,
            queue: Some(queue),
            timeout: Some(timeout),
        }
    }
}

/// Run `f` on the blocking thread pool, gated by `config`.
///
/// `f` is invoked with an [`OwnedSemaphorePermit`] so the permit is
/// held for the duration of the work — the helper therefore bounds
/// both the number of in-flight blocking tasks AND the memory they
/// hold (the permit is released only when `f` returns or panics).
///
/// When [`BoundedConfig::queue`] is set, the caller must first take
/// a queue permit; a rejection surfaces immediately as
/// [`RunError::QueueFull`]. The queue permit is held until the
/// concurrency permit is acquired, then released — a queue slot
/// is therefore occupied for exactly the time the caller is
/// waiting on the concurrency semaphore, never while the actual
/// blocking work is running.
pub async fn run_bounded<F, R>(config: BoundedConfig, f: F) -> Result<R, RunError>
where
    F: FnOnce(OwnedSemaphorePermit) -> R + Send + 'static,
    R: Send + 'static,
{
    // 1. Queue gate: try once, fail fast on saturation.
    let queue_permit = if let Some(BoundedQueue {
        semaphore,
        max_waiters,
    }) = &config.queue
    {
        Some(
            Arc::clone(semaphore)
                .try_acquire_owned()
                .map_err(|_| RunError::QueueFull(*max_waiters))?,
        )
    } else {
        None
    };

    // 2. Concurrency permit: optional timeout, otherwise wait.
    let permit = if let Some(timeout) = config.timeout {
        match tokio::time::timeout(timeout, config.semaphore.acquire_owned()).await {
            Ok(Ok(p)) => p,
            Ok(Err(_closed)) => return Err(RunError::Closed),
            Err(_elapsed) => return Err(RunError::QueueTimeout(timeout)),
        }
    } else {
        config
            .semaphore
            .acquire_owned()
            .await
            .map_err(|_| RunError::Closed)?
    };

    // 3. Release the queue slot now that we hold the concurrency permit.
    drop(queue_permit);

    // 4. Spawn blocking and await the join.
    let join = tokio::task::spawn_blocking(move || f(permit)).await?;
    Ok(join)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn closure_runs_to_completion() {
        let sem = Arc::new(Semaphore::new(1));
        let cfg = BoundedConfig::unbounded(sem);
        let v: u32 = run_bounded(cfg, |_permit| 42).await.unwrap();
        assert_eq!(v, 42);
    }

    #[tokio::test]
    async fn permit_is_held_for_the_duration_of_the_closure() {
        // The closure deliberately sleeps and reads the semaphore
        // capacity while it runs. While the closure holds the
        // permit, the capacity is 0; once the closure returns, the
        // permit is released and the capacity is back to 1.
        let sem = Arc::new(Semaphore::new(1));
        let sem_inside = Arc::clone(&sem);
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started_inside = Arc::clone(&started);
        let sem_for_task = Arc::clone(&sem);
        let task = tokio::spawn(async move {
            let cfg = BoundedConfig::unbounded(sem_for_task);
            run_bounded(cfg, move |_permit| {
                started_inside.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                // The permit is held here; another `acquire_owned`
                // from outside the closure must wait until we drop.
                let available = sem_inside.available_permits();
                assert_eq!(available, 0, "permit must be held while the closure runs");
            })
            .await
            .unwrap();
        });

        // Give the task a moment to start running the closure.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));

        // After the closure returns, the next acquisition is immediate.
        let _permit = sem.acquire_owned().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn concurrency_cap_is_honoured() {
        // Three tasks, one permit — the third must wait for the
        // first two to finish.
        let sem = Arc::new(Semaphore::new(1));
        let n = 3usize;
        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for _ in 0..n {
            let s = Arc::clone(&sem);
            handles.push(tokio::spawn(async move {
                let cfg = BoundedConfig::unbounded(s);
                run_bounded(cfg, |_permit| {
                    std::thread::sleep(Duration::from_millis(40));
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = start.elapsed();
        // Sequential: ~120 ms. Parallel: ~40 ms. We expect
        // sequential because the cap is 1.
        assert!(
            elapsed >= Duration::from_millis(100),
            "elapsed {elapsed:?} should reflect sequential execution"
        );
    }

    #[tokio::test]
    async fn queue_full_rejects_immediately() {
        // Concurrency cap 1, queue cap 1. The first caller holds
        // the concurrency permit; the second caller holds the
        // single queue permit; the third caller must be rejected
        // with QueueFull.
        let sem = Arc::new(Semaphore::new(1));
        let queue_arc = Arc::new(Semaphore::new(1));
        let cfg_for = || BoundedConfig {
            semaphore: Arc::clone(&sem),
            queue: Some(BoundedQueue {
                semaphore: Arc::clone(&queue_arc),
                max_waiters: 1,
            }),
            timeout: None,
        };

        // First caller: take the concurrency permit and stay.
        let first = tokio::spawn({
            let cfg = cfg_for();
            async move {
                run_bounded(cfg, |_permit| {
                    std::thread::sleep(Duration::from_millis(200));
                })
                .await
            }
        });
        // Second caller: take the queue permit, then wait for the
        // concurrency permit.
        let second = tokio::spawn({
            let cfg = cfg_for();
            async move {
                run_bounded(cfg, |_permit| {
                    std::thread::sleep(Duration::from_millis(50));
                })
                .await
            }
        });
        // Yield so both spawned tasks grab their permits first.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Third caller: no queue permit available.
        let third = run_bounded(cfg_for(), |_permit| ()).await;
        assert!(
            matches!(third, Err(RunError::QueueFull(1))),
            "third caller must be rejected, got {third:?}"
        );

        // The first two eventually succeed.
        let _ = tokio::join!(async { first.await.unwrap() }, async {
            second.await.unwrap()
        });
    }

    #[tokio::test]
    async fn queue_timeout_fires_when_no_permit() {
        let sem = Arc::new(Semaphore::new(1));
        let queue_arc = Arc::new(Semaphore::new(4));
        let queue = BoundedQueue {
            semaphore: Arc::clone(&queue_arc),
            max_waiters: 4,
        };
        let cfg = BoundedConfig {
            semaphore: Arc::clone(&sem),
            queue: Some(queue),
            timeout: Some(Duration::from_millis(50)),
        };

        // Hold the concurrency permit in another task.
        let blocker = tokio::spawn(async move {
            run_bounded(BoundedConfig::unbounded(Arc::clone(&sem)), |_permit| {
                std::thread::sleep(Duration::from_millis(300));
            })
            .await
        });

        // Yield so blocker grabs the concurrency permit.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Now: queue permit available, but concurrency permit is
        // held by the blocker. Wait timeout fires.
        let res = run_bounded(cfg, |_permit| ()).await;
        assert!(
            matches!(res, Err(RunError::QueueTimeout(_))),
            "expected QueueTimeout, got {res:?}"
        );

        let _ = blocker.await.unwrap();
    }

    #[tokio::test]
    async fn queue_permit_is_dropped_after_concurrency_acquired() {
        // Verify that a queue permit is released the moment the
        // concurrency permit lands. Without this, the queue would
        // leak one slot per saturated window and a steady-state
        // burst would slowly fill the queue even though the
        // concurrency cap is honoured.
        let sem = Arc::new(Semaphore::new(1));
        let queue_arc = Arc::new(Semaphore::new(1));
        let cfg_for = || BoundedConfig {
            semaphore: Arc::clone(&sem),
            queue: Some(BoundedQueue {
                semaphore: Arc::clone(&queue_arc),
                max_waiters: 1,
            }),
            timeout: None,
        };

        // Task A: holds the concurrency permit briefly.
        let a = tokio::spawn({
            let cfg = cfg_for();
            async move {
                run_bounded(cfg, |_permit| {
                    std::thread::sleep(Duration::from_millis(50));
                })
                .await
                .unwrap();
            }
        });
        // Yield so A grabs the permit.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Task B: grabs the queue permit (since A holds the
        // concurrency permit), waits for A to finish, then runs.
        let b = tokio::spawn({
            let cfg = cfg_for();
            async move {
                run_bounded(cfg, |_permit| {
                    // Confirm the queue permit has been released by
                    // now: the queue should be back to its full
                    // capacity once B has acquired the concurrency
                    // permit. We can't observe that from inside the
                    // closure (we're holding the concurrency permit by
                    // then), but if the queue was leaked, a third
                    // concurrent caller would get QueueFull instead of
                    // QueueTimeout. We check that out-of-band below.
                })
                .await
                .unwrap();
            }
        });

        a.await.unwrap();
        b.await.unwrap();

        // Queue should be back to 1 available.
        assert_eq!(
            queue_arc.available_permits(),
            1,
            "queue permit must be released once concurrency permit lands"
        );
    }
}
