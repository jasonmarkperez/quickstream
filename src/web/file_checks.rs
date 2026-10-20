//! File checks for the continuous stream that never hold up its writes (#117): a file on a
//! network share that hangs rather than unmounting can take seconds to stat, so each check runs
//! on a helper thread, at most one in flight per path, and the stream polls for its answer while
//! it sends paced silence. A helper that answered takes the next check that comes within
//! `HELPER_IDLE`, so the rechecks of a missing queue don't start a thread per file each time.

use std::collections::{HashMap, VecDeque};
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
/// How many paths are remembered before old answers are first dropped; after that, when the map
/// has doubled since, so a large queue whose answers are all recent isn't walked on every check.
const PRUNE_AT: usize = 1024;
/// How old an answer is when it is dropped then.
const PRUNE_AGE: Duration = Duration::from_secs(60);
/// How long a helper thread waits for another check before it ends: longer than the stream's
/// recheck of a missing queue, so rechecks run on the helpers that answered before.
const HELPER_IDLE: Duration = Duration::from_secs(5);

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

/// A path to check, and the check to run.
type Job = (PathBuf, FileCheck);

#[derive(Default)]
struct Checks {
    entries: HashMap<PathBuf, Entry>,
    /// How many entries were left after the last prune.
    pruned_len: usize,
    /// Checks handed to idle helpers that haven't taken them yet, oldest first.
    queued: VecDeque<Job>,
    /// Helpers waiting for a check.
    idle: usize,
}

#[derive(Default)]
struct Shared {
    checks: Mutex<Checks>,
    /// Notified when a check answers.
    answered: Condvar,
    /// Notified when a check is queued for an idle helper.
    work: Condvar,
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
        let mut checks = self.shared.checks.lock().unwrap();
        loop {
            match checks.entries.get(path) {
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
                    if checks.entries.len() >= PRUNE_AT.max(2 * checks.pruned_len) {
                        checks.entries.retain(|_, entry| match entry {
                            Entry::InFlight(_) => true,
                            Entry::Answered { at, .. } => at.elapsed() < PRUNE_AGE,
                        });
                        checks.pruned_len = checks.entries.len();
                    }
                    checks
                        .entries
                        .insert(path.to_path_buf(), Entry::InFlight(Instant::now()));
                    if !self.dispatch(&mut checks, (path.to_path_buf(), self.check())) {
                        checks.entries.remove(path);
                        return FileStatus::Unanswered;
                    }
                }
            }
            let now = Instant::now();
            if now >= wait_until {
                return FileStatus::Pending;
            }
            checks = self.shared.answered.wait_timeout(checks, wait_until - now).unwrap().0;
        }
    }

    /// Hand `job` to an idle helper, else start one (a helper stuck on a hung share takes no
    /// other check). Returns whether it is in a helper's hands.
    fn dispatch(&self, checks: &mut Checks, job: Job) -> bool {
        if checks.idle > checks.queued.len() {
            checks.queued.push_back(job);
            self.shared.work.notify_one();
            return true;
        }
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("qs-file-check".to_string())
            .spawn(move || helper(shared, job))
            .is_ok()
    }
}

/// Run `job`, record its answer, then take the checks queued for idle helpers until none comes
/// for `HELPER_IDLE`.
fn helper(shared: Arc<Shared>, job: Job) {
    let mut job = Some(job);
    while let Some((path, check)) = job.take() {
        // A check that panics counts as a missing file, so its path is checked again.
        let exists = std::panic::catch_unwind(AssertUnwindSafe(|| check(&path))).unwrap_or(false);
        let at = Instant::now();
        let mut checks = shared.checks.lock().unwrap();
        checks.entries.insert(path, Entry::Answered { exists, at });
        shared.answered.notify_all();
        checks.idle += 1;
        let deadline = Instant::now() + HELPER_IDLE;
        loop {
            if let Some(next) = checks.queued.pop_front() {
                job = Some(next);
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            checks = shared.work.wait_timeout(checks, deadline - now).unwrap().0;
        }
        checks.idle -= 1;
    }
}
