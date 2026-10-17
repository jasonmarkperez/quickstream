//! Registry of the UI's background worker threads, so they can be joined on shutdown (issue #5).

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Background threads spawned by the UI. Finished threads are reaped on every spawn; on shutdown
/// [`WorkerRegistry::join_all`] waits for the rest up to a deadline and detaches stragglers so
/// quitting stays fast even when a thread is stuck in slow I/O (e.g. a network share).
#[derive(Default)]
pub struct WorkerRegistry {
    handles: Vec<(&'static str, JoinHandle<()>)>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a named worker thread and keep its handle. The thread must exit on its own once its
    /// stop signal fires (stop flag, dropped channel); the registry only waits for it.
    pub fn spawn<F>(&mut self, name: &'static str, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.reap();
        match std::thread::Builder::new().name(format!("qs-{}", name)).spawn(f) {
            Ok(handle) => self.handles.push((name, handle)),
            Err(e) => crate::log_error!("workers", "Failed to spawn '{}' worker thread: {}", name, e),
        }
    }

    /// Join threads that have already finished.
    fn reap(&mut self) {
        let mut i = 0;
        while i < self.handles.len() {
            if self.handles[i].1.is_finished() {
                let (name, handle) = self.handles.swap_remove(i);
                if handle.join().is_err() {
                    crate::log_warn!("workers", "'{}' worker thread panicked", name);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Number of worker threads still running.
    pub fn live_count(&mut self) -> usize {
        self.reap();
        self.handles.len()
    }

    /// Wait up to `timeout` in total for every worker to finish, joining each one. Threads still
    /// running at the deadline are detached and logged. Returns how many were left running.
    pub fn join_all(&mut self, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            self.reap();
            if self.handles.is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let stragglers: Vec<&str> = self.handles.iter().map(|(name, _)| *name).collect();
        if !stragglers.is_empty() {
            crate::log_warn!(
                "workers",
                "{} worker thread(s) still running after {:?}, detaching: {:?}",
                stragglers.len(),
                timeout,
                stragglers
            );
        }
        let left = self.handles.len();
        self.handles.clear();
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_all_joins_finished_and_detaches_stuck() {
        let mut reg = WorkerRegistry::new();
        reg.spawn("quick", || {});
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        reg.spawn("stuck", move || {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        });
        let start = Instant::now();
        assert_eq!(reg.join_all(Duration::from_millis(100)), 1);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(reg.live_count(), 0);
        drop(tx); // lets the detached thread exit
    }

    #[test]
    fn test_panicking_worker_is_reaped() {
        let mut reg = WorkerRegistry::new();
        reg.spawn("panics", || panic!("worker panic (expected in test)"));
        assert_eq!(reg.join_all(Duration::from_secs(2)), 0);
    }
}
