//! File checks for the continuous stream that never hold up its writes (#117): a file on a
//! network share that hangs rather than unmounting can take seconds to stat, so each check runs
//! on a helper thread, at most one in flight per path and at most `MAX_HELPERS` at once, and the
//! stream polls for the answers while it sends paced silence. A look through the queue checks
//! the files after a slow one ahead of time, so a playable file past hung ones is found within
//! about one deadline. A helper that answered takes the next check that comes within
//! `HELPER_IDLE`, so the rechecks of a missing queue don't start a thread per file each time.

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What tells whether a file is there: `Path::exists`, unless a test replaces it.
pub(super) type FileCheck = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// How long a check may go unanswered (on a helper, or waiting for one) before the stream passes
/// its file over. The check stays in flight (a stat can't be called off), so no second one starts
/// for that path; its answer is used once it comes, by the next look too (see `LATE_ANSWER_KEEP`).
pub(super) const FILE_CHECK_DEADLINE: Duration = Duration::from_secs(2);
/// The most helper threads at once. A check that hangs holds its helper until it answers, so a
/// hung share holds this many threads at most; a check past them waits for a helper to free (and
/// counts as unanswered once it has waited `FILE_CHECK_DEADLINE`).
const MAX_HELPERS: usize = 16;
/// How many unanswered checks a look has in flight at most, ahead of the first file it waits on.
const LOOKAHEAD: usize = MAX_HELPERS;
/// How long a look waits on a file's check before it checks the files after it too, so a queue
/// on a working share is checked one file at a time.
const LOOKAHEAD_AFTER: Duration = Duration::from_millis(250);
/// How long an answer that came after its deadline still counts for the next look that asks,
/// however long since it began: on a share slower than the deadline, a check made afresh for each
/// look would never answer in time.
const LATE_ANSWER_KEEP: Duration = Duration::from_secs(60);
/// How many paths are remembered before old answers are first dropped; after that, when the map
/// has doubled since, so a large queue whose answers are all recent isn't walked on every check.
const PRUNE_AT: usize = 1024;
/// How old an answer is when it is dropped then.
const PRUNE_AGE: Duration = Duration::from_secs(60);
/// How long a helper thread waits for another check before it ends: longer than the stream's
/// recheck of a missing queue, so rechecks run on the helpers that answered before.
const HELPER_IDLE: Duration = Duration::from_secs(5);

/// What a look through the queue found, by position in the order it looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LookOutcome {
    /// The first file there, every one before it missing or unanswered.
    Found(usize),
    /// Every file is missing or unanswered.
    NoneFound,
    /// A file's check hasn't answered yet (nor gone past its deadline): look again on a later
    /// pass.
    Pending,
}

/// A look's outcome and the files it passed over before deciding: their positions, and whether
/// their checks went unanswered (rather than finding them missing).
pub(super) struct Look {
    pub(super) outcome: LookOutcome,
    pub(super) passed_over: Vec<(usize, bool)>,
}

enum Entry {
    /// Asked for at this time: on a helper, or waiting for one.
    InFlight(Instant),
    /// `late`: given after `FILE_CHECK_DEADLINE`, and not used by a look yet.
    Answered { exists: bool, at: Instant, late: bool },
}

/// What a look knows about one file.
enum Status {
    Exists,
    Missing,
    /// In flight since this time.
    Pending(Instant),
    Unanswered,
}

/// A path to check, and the check to run.
type Job = (PathBuf, FileCheck);

#[derive(Default)]
struct Checks {
    entries: HashMap<PathBuf, Entry>,
    /// How many entries were left after the last prune.
    pruned_len: usize,
    /// Checks no helper has taken yet, oldest first.
    queued: VecDeque<Job>,
    /// Helpers waiting for a check.
    idle: usize,
    /// Helper threads running or being started.
    helpers: usize,
}

#[derive(Default)]
struct Shared {
    checks: Mutex<Checks>,
    /// Notified when a check answers.
    answered: Condvar,
    /// Notified when a check is queued.
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

    /// Look through `len` files (`path_at` gives the one at each position, in order) for the
    /// first that exists, from answers given since `since`. Files without one get a check on a
    /// helper thread (unless one of theirs is in flight); the files after the first unanswered
    /// one are checked ahead once it has taken `LOOKAHEAD_AFTER`. Waits for answers until
    /// `wait_until` at most, so the caller never waits longer than it allows.
    pub(super) fn look<'p>(
        &self,
        len: usize,
        path_at: impl Fn(usize) -> &'p Path,
        since: Instant,
        wait_until: Instant,
    ) -> Look {
        let check = self.check();
        let mut tried_spawning = false;
        let mut checks = self.shared.checks.lock().unwrap();
        loop {
            let (look, late) = walk(&mut checks, len, &path_at, since, &check);
            if look.outcome != LookOutcome::Pending {
                // A late answer counts once: the next look asks afresh.
                for p in late {
                    if let Some(Entry::Answered { late, .. }) = checks.entries.get_mut(path_at(p)) {
                        *late = false;
                    }
                }
                return look;
            }
            if !checks.queued.is_empty() && checks.idle > 0 {
                self.shared.work.notify_all();
            }
            // Helpers for the checks the idle ones won't take, started with the lock released.
            let wanted = checks
                .queued
                .len()
                .saturating_sub(checks.idle)
                .min(MAX_HELPERS.saturating_sub(checks.helpers));
            if wanted > 0 && !tried_spawning {
                tried_spawning = true;
                checks.helpers += wanted;
                drop(checks);
                let started = (0..wanted).filter(|_| self.spawn_helper()).count();
                checks = self.shared.checks.lock().unwrap();
                checks.helpers -= wanted - started;
                continue;
            }
            let now = Instant::now();
            if now >= wait_until {
                return look;
            }
            checks = self.shared.answered.wait_timeout(checks, wait_until - now).unwrap().0;
        }
    }

    /// Start checking `path` unless an answer given since `since` is known or a check is in
    /// flight, without waiting for it: the stream asks for the next file as a track starts, so
    /// the answer is there when the track ends.
    pub(super) fn prefetch(&self, path: &Path, since: Instant) {
        self.look(1, |_| path, since, Instant::now());
    }

    /// Start a helper thread, which takes the queued checks. Returns whether it started.
    fn spawn_helper(&self) -> bool {
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("qs-file-check".to_string())
            .spawn(move || helper(shared))
            .is_ok()
    }
}

/// One walk of a look, under the lock: what it found and the positions whose late answers it
/// used.
fn walk<'p>(
    checks: &mut Checks,
    len: usize,
    path_at: &impl Fn(usize) -> &'p Path,
    since: Instant,
    check: &FileCheck,
) -> (Look, Vec<usize>) {
    let now = Instant::now();
    let mut passed_over = Vec::new();
    let mut late_used = Vec::new();
    // When the first file still waited on was asked for, and how many are waited on.
    let mut first_pending: Option<Instant> = None;
    let mut pending = 0;
    for p in 0..len {
        let path = path_at(p);
        let status = match checks.entries.get(path) {
            Some(Entry::Answered { exists, at, late })
                if *at >= since || (*late && now.duration_since(*at) < LATE_ANSWER_KEEP) =>
            {
                if *at < since {
                    late_used.push(p);
                }
                if *exists {
                    Status::Exists
                } else {
                    Status::Missing
                }
            }
            Some(Entry::InFlight(asked)) => {
                if now.duration_since(*asked) >= FILE_CHECK_DEADLINE {
                    Status::Unanswered
                } else {
                    Status::Pending(*asked)
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
                checks.entries.insert(path.to_path_buf(), Entry::InFlight(now));
                checks.queued.push_back((path.to_path_buf(), check.clone()));
                Status::Pending(now)
            }
        };
        match status {
            Status::Exists => {
                if first_pending.is_none() {
                    let look = Look {
                        outcome: LookOutcome::Found(p),
                        passed_over,
                    };
                    return (look, late_used);
                }
                // Played only once the files before it have answered.
                break;
            }
            Status::Missing | Status::Unanswered => {
                if first_pending.is_none() {
                    passed_over.push((p, matches!(status, Status::Unanswered)));
                }
            }
            Status::Pending(asked) => {
                let first = *first_pending.get_or_insert(asked);
                pending += 1;
                if pending >= LOOKAHEAD || now.duration_since(first) < LOOKAHEAD_AFTER {
                    break;
                }
            }
        }
    }
    let outcome = if first_pending.is_some() {
        LookOutcome::Pending
    } else {
        LookOutcome::NoneFound
    };
    (Look { outcome, passed_over }, late_used)
}

/// Take the queued checks and record their answers until none comes for `HELPER_IDLE`.
fn helper(shared: Arc<Shared>) {
    let mut checks = shared.checks.lock().unwrap();
    loop {
        if let Some((path, check)) = checks.queued.pop_front() {
            drop(checks);
            // A check that panics counts as a missing file (looked at again by a later look, as
            // any missing file is).
            let exists = std::panic::catch_unwind(AssertUnwindSafe(|| check(&path))).unwrap_or(false);
            checks = shared.checks.lock().unwrap();
            let at = Instant::now();
            let late = matches!(
                checks.entries.get(&path),
                Some(Entry::InFlight(asked)) if at.duration_since(*asked) >= FILE_CHECK_DEADLINE
            );
            checks.entries.insert(path, Entry::Answered { exists, at, late });
            shared.answered.notify_all();
            continue;
        }
        checks.idle += 1;
        let deadline = Instant::now() + HELPER_IDLE;
        while checks.queued.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            checks = shared.work.wait_timeout(checks, deadline - now).unwrap().0;
        }
        checks.idle -= 1;
        if checks.queued.is_empty() {
            checks.helpers -= 1;
            return;
        }
    }
}
