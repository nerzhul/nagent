//! FIFO inference worker.
//!
//! The worker owns a [`mpsc::Receiver`] of [`InferenceJob`]s and an
//! [`Arc`] to a [`WhisperBackend`]. It pops jobs in order, calls
//! `backend.infer(...)`, and forwards the result through the job's oneshot.
//!
//! Isolation between sessions is structural: every job carries its own
//! `session_id` and its own `response_tx`; the worker never reads or
//! mutates any per-session state.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::backend::{BackendError, WhisperBackend};
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

    async fn handle_one(backend: &Arc<dyn WhisperBackend>, job: InferenceJob) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendError, WhisperBackend};
    use crate::job::{InferRequest, InferResponse};
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    /// Mock backend that echoes `format!("session={}", req.session_id)`.
    /// Used to assert that the worker preserves session identity.
    #[derive(Debug)]
    struct EchoBackend;

    #[async_trait::async_trait]
    impl WhisperBackend for EchoBackend {
        async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError> {
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
        let backend: Arc<dyn WhisperBackend> = Arc::new(EchoBackend);
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
        let backend: Arc<dyn WhisperBackend> = Arc::new(EchoBackend);
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
}
