//! Whether the folder a mix picker or Seed Radio start prefers is still there, asked on the
//! Autopilot scan's worker so a slow or hung share never stalls the key (issue #103). One check
//! per path runs at a time: a start whose folder is already being checked (the key pressed again
//! while a share hangs) waits for that answer instead of adding a check of its own, and gives up
//! once its scan is stopped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Whether a path is there: [`folder_exists`] except in tests, which hold the check back to stand
/// in for a slow share (`AppUI::set_path_check`).
pub type PathCheck = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// Whether `path` is there, following symlinks as [`Path::exists`] does. A lookup that fails for
/// another reason than a missing path (a timed-out share, no permission) counts as not there too,
/// and is logged. Blocking: call it on a worker thread, never on the UI thread.
pub fn folder_exists(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                crate::log_warn!(
                    "autopilot",
                    "Couldn't look up '{}': {}; using the fallback folder",
                    path.display(),
                    err
                );
            }
            false
        }
    }
}

/// How often a start waiting for another worker's check looks at its stop flag.
const STOP_POLL: Duration = Duration::from_millis(20);

/// One running check's answer, for the workers waiting for it.
#[derive(Default)]
struct Answer {
    exists: Mutex<Option<bool>>,
    ready: Condvar,
}

/// The folder checks Autopilot workers are running, by path. Clones share them.
#[derive(Clone)]
pub(super) struct FolderChecks {
    check: PathCheck,
    running: Arc<Mutex<HashMap<PathBuf, Arc<Answer>>>>,
}

/// Publishes the owner's answer and frees the path, also when the check panics (as not there).
struct Publish<'a> {
    checks: &'a FolderChecks,
    path: &'a Path,
    answer: Arc<Answer>,
    exists: bool,
}

impl Drop for Publish<'_> {
    fn drop(&mut self) {
        self.checks.running.lock().unwrap().remove(self.path);
        *self.answer.exists.lock().unwrap() = Some(self.exists);
        self.answer.ready.notify_all();
    }
}

impl FolderChecks {
    pub(super) fn new(check: PathCheck) -> Self {
        FolderChecks {
            check,
            running: Arc::default(),
        }
    }

    /// Whether `path` is there; `None` if `stop` is set while this waits for another worker's
    /// check of it. Blocking: call it on a worker thread, never on the UI thread.
    pub(super) fn exists(&self, path: &Path, stop: &AtomicBool) -> Option<bool> {
        let (answer, owner) = {
            let mut running = self.running.lock().unwrap();
            match running.get(path) {
                Some(answer) => (answer.clone(), false),
                None => {
                    let answer = Arc::new(Answer::default());
                    running.insert(path.to_path_buf(), answer.clone());
                    (answer, true)
                }
            }
        };
        if owner {
            let mut publish = Publish {
                checks: self,
                path,
                answer,
                exists: false,
            };
            publish.exists = (self.check)(path);
            return Some(publish.exists);
        }
        let mut exists = answer.exists.lock().unwrap();
        loop {
            if let Some(exists) = *exists {
                return Some(exists);
            }
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            exists = answer.ready.wait_timeout(exists, STOP_POLL).unwrap().0;
        }
    }
}
