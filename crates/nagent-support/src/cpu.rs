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
//! The future returns `Result<R, RunError>`. The two failure modes
//! are:
//!
//! - [`RunError::Closed`] — the semaphore was closed before a
//!   permit could be acquired (only happens if the owning
//!   subsystem is being torn down). Callers should treat this as
//!   "service unavailable".
//! - [`RunError::Join`] — the blocking task panicked. Callers
//!   should treat this as an internal error; the underlying
//!   library (Argon2 / pdf-extract) has violated its contract.

use std::sync::Arc;

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
}

/// Run `f` on the blocking thread pool, gated by `semaphore`.
///
/// `f` is invoked with an [`OwnedSemaphorePermit`] so the permit is
/// held for the duration of the work — the helper therefore bounds
/// both the number of in-flight blocking tasks AND the memory they
/// hold (the permit is released only when `f` returns or panics).
///
/// # Example
///
/// ```ignore
/// use std::sync::Arc;
/// use tokio::sync::Semaphore;
///
/// let sem = Arc::new(Semaphore::new(4));
/// let bytes = run_bounded(sem, |_permit| {
///     // CPU-heavy or blocking I/O work.
///     vec![0u8; 1024]
/// }).await?;
/// ```
pub async fn run_bounded<F, R>(semaphore: Arc<Semaphore>, f: F) -> Result<R, RunError>
where
    F: FnOnce(OwnedSemaphorePermit) -> R + Send + 'static,
    R: Send + 'static,
{
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_| RunError::Closed)?;
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
        let v: u32 = run_bounded(sem, |_permit| 42).await.unwrap();
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
            run_bounded(sem_for_task, move |_permit| {
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
                run_bounded(s, |_permit| {
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
}
