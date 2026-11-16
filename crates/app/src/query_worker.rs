//! One thread for message-list queries, which only ever runs the newest.
//!
//! Every keystroke in the search box asks for a list. Handed to the background
//! executor, each became its own task, and all of them queued on the store's
//! one connection: typing "postmortem" ran ten queries in turn, starting with
//! "p" — which matches most of the mailbox and is the slowest of the ten — and
//! the answer to what had actually been typed waited for all nine before it.
//!
//! Here there is at most one query running and one waiting. A request that
//! arrives while one is waiting replaces it: the replaced request's receiver
//! sees its sender dropped, which its caller reads as "superseded", which it
//! was. So typing ten characters costs, at worst, whatever query was already
//! running plus the last one.

use std::sync::{Arc, Condvar, Mutex};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct QueryWorker {
    slot: Arc<(Mutex<Option<Job>>, Condvar)>,
}

impl QueryWorker {
    pub fn new() -> Self {
        let slot: Arc<(Mutex<Option<Job>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
        let worker = slot.clone();
        std::thread::Builder::new()
            .name("spool-list-query".into())
            .spawn(move || {
                let (lock, ready) = &*worker;
                loop {
                    let job = {
                        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(job) = pending.take() {
                                break job;
                            }
                            pending = ready.wait(pending).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    job();
                }
            })
            .expect("spawning the list query thread");
        Self { slot }
    }

    /// Queues `query` in place of whatever was waiting, and returns its answer.
    ///
    /// `None` when a newer request took its place before it ran.
    pub fn run<T, F>(&self, query: F) -> impl std::future::Future<Output = Option<T>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (send, receive) = smol::channel::bounded(1);
        let job: Job = Box::new(move || {
            // Nobody waiting any more — the request was dropped — is fine.
            let _ = send.try_send(query());
        });
        let (lock, ready) = &*self.slot;
        // Dropping the job this replaces drops its sender, which is what
        // tells its caller it was superseded.
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(job);
        ready.notify_one();
        async move { receive.recv().await.ok() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_request_replaced_while_waiting_never_runs() {
        let worker = QueryWorker::new();
        // Hold the worker busy so the next two requests both have to wait.
        let (release, hold) = mpsc::channel::<()>();
        let busy = worker.run(move || hold.recv().ok());
        std::thread::sleep(Duration::from_millis(20));

        let replaced = worker.run(|| "replaced");
        let newest = worker.run(|| "newest");
        release.send(()).unwrap();

        assert_eq!(smol::block_on(busy), Some(Some(())));
        assert_eq!(smol::block_on(replaced), None, "superseded before it ran");
        assert_eq!(smol::block_on(newest), Some("newest"));
    }

    #[test]
    fn requests_spaced_out_all_run() {
        let worker = QueryWorker::new();
        for n in 0..5 {
            assert_eq!(smol::block_on(worker.run(move || n * 2)), Some(n * 2));
        }
    }
}
