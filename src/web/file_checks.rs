//! File checks for the continuous stream that never hold up its writes (#117): a file on a
//! network share that hangs rather than unmounting can take seconds to stat, so each check runs
//! on a helper thread, at most one in flight per path, and the stream polls for its answer while
//! it sends paced silence.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What tells whether a file is there: `Path::exists`, unless a test replaces it.
pub(super) type FileCheck = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// How long a check may go unanswered before the stream passes its file over. The check stays in
/// flight (a stat can't be called off), so no second one starts for that path; its answer is
/// used once it comes.
pub(super) const FILE_CHECK_DEADLINE: Duration = Duration::from_secs(2);
/// How many paths are remembered before old answers are dropped.
const PRUNE_AT: usize = 1024;
/// How old an answer is when it is dropped then.
const PRUNE_AGE: Duration = Duration::from_secs(60);

/// What is known about a file, as [`FileChecks::status`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileStatus {
    Exists,
    Missing,
    /// Its check hasn't answered yet: ask again on a later pass.
    Pending,
    /// Its check has gone unanswered for `FILE_CHECK_DEADLINE` (a hung share), or couldn't start.
    Unanswered,
}

enum Entry {
    InFlight(Instant),
    Answered { exists: bool, at: Instant },
}

#[derive(Default)]
struct Shared {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    answered: Condvar,
}

/// The continuous stream's file checks, shared by its connections (a reconnect starts no second
/// check of a path whose first is still in flight).
#[derive(Default)]
pub(super) struct FileChecks {
    /// `None`: `Path::exists`.
    check: Mutex<Option<FileCheck>>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for FileChecks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChecks").finish_non_exhaustive()
    }
}

impl FileChecks {
    /// Check files with `check` from now on.
    pub(super) fn set_check(&self, check: FileCheck) {
        *self.check.lock().unwrap() = Some(check);
    }

    fn check(&self) -> FileCheck {
        self.check
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Arc::new(|path: &Path| path.exists()))
    }

    /// Whether `path` exists, from an answer given since `since`. Without one, a check is started
    /// on a helper thread (unless one of `path` is in flight) and waited for until `wait_until`
    /// at most, so the caller never waits longer than it allows.
    pub(super) fn status(&self, path: &Path, since: Instant, wait_until: Instant) -> FileStatus {
        let mut entries = self.shared.entries.lock().unwrap();
        loop {
            match entries.get(path) {
                Some(Entry::Answered { exists, at }) if *at >= since => {
                    return if *exists {
                        FileStatus::Exists
                    } else {
                        FileStatus::Missing
                    };
                }
                Some(Entry::InFlight(started)) => {
                    if started.elapsed() >= FILE_CHECK_DEADLINE {
                        return FileStatus::Unanswered;
                    }
                }
                _ => {
                    if entries.len() >= PRUNE_AT {
                        entries.retain(|_, entry| match entry {
                            Entry::InFlight(_) => true,
                            Entry::Answered { at, .. } => at.elapsed() < PRUNE_AGE,
                        });
                    }
                    entries.insert(path.to_path_buf(), Entry::InFlight(Instant::now()));
                    if !self.spawn(path) {
                        entries.remove(path);
                        return FileStatus::Unanswered;
                    }
                }
            }
            let now = Instant::now();
            if now >= wait_until {
                return FileStatus::Pending;
            }
            entries = self.shared.answered.wait_timeout(entries, wait_until - now).unwrap().0;
        }
    }

    /// Check `path` on a helper thread, which records the answer. Returns whether it started.
    fn spawn(&self, path: &Path) -> bool {
        let (check, shared, path) = (self.check(), self.shared.clone(), path.to_path_buf());
        std::thread::Builder::new()
            .name("qs-file-check".to_string())
            .spawn(move || {
                // A check that panics counts as a missing file, so its path is checked again.
                let exists = std::panic::catch_unwind(AssertUnwindSafe(|| check(&path))).unwrap_or(false);
                let at = Instant::now();
                shared
                    .entries
                    .lock()
                    .unwrap()
                    .insert(path, Entry::Answered { exists, at });
                shared.answered.notify_all();
            })
            .is_ok()
    }
}
