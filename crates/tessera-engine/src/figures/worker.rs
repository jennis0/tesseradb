//! The one thread the figures' background work runs on: writes to the cache directory and reads of
//! denied rows' labels, none of which a request waits for. A queue rather than a thread per job,
//! so a burst of work takes one thread and runs in order.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// The thread and its queue, started on the first job.
#[derive(Default)]
pub(crate) struct Worker {
    running: Mutex<Option<(Sender<Job>, JoinHandle<()>)>>,
}

impl Worker {
    /// Queue `job`. Where the thread cannot be started, the job is skipped, and said so.
    pub(crate) fn submit(&self, job: impl FnOnce() + Send + 'static) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.is_none() {
            let (tx, rx) = channel::<Job>();
            let started = std::thread::Builder::new()
                .name("tessera-figures".to_string())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                });
            match started {
                Ok(handle) => *running = Some((tx, handle)),
                Err(error) => {
                    tracing::warn!(%error, "the figures' background thread could not be started; its work is skipped");
                    return;
                }
            }
        }
        if let Some((tx, _)) = running.as_ref() {
            let _ = tx.send(Box::new(job));
        }
    }

    /// Run every queued job and stop the thread.
    pub(crate) fn finish(&self) {
        let taken = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((tx, handle)) = taken {
            drop(tx);
            let _ = handle.join();
        }
    }

    /// Wait until every job queued so far has run: for a test that reads what the jobs wrote.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn drain(&self) {
        let (done, finished) = channel::<()>();
        self.submit(move || {
            let _ = done.send(());
        });
        let _ = finished.recv();
    }
}
