//! Keys that need to know whether a folder is there before they act (the mix picker, Seed
//! Radio, `~`): a `folder-check` worker looks it up and `poll_folder_checks` acts on the answer,
//! so a slow or hung share never stalls the UI thread (issue #103).

use std::path::{Path, PathBuf};

use super::AppUI;

/// What a path is on disk, as a `folder-check` worker finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Missing,
    Dir,
    /// A file, or anything else that is not a folder.
    Other,
}

impl PathKind {
    /// Look `path` up, following symlinks as [`Path::exists`] and [`Path::is_dir`] do.
    /// Blocking: call it on a worker thread, never on the UI thread.
    pub fn of(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => PathKind::Dir,
            Ok(_) => PathKind::Other,
            Err(_) => PathKind::Missing,
        }
    }
}

/// Looks a path up for a `folder-check` worker: [`PathKind::of`] except in tests, which hold
/// the check back to stand in for a slow share (`AppUI::set_path_check`).
pub type PathCheck = std::sync::Arc<dyn Fn(&Path) -> PathKind + Send + Sync>;

/// Where the browser is: the shown folder, whether Music Homes is shown, and the folder being
/// opened.
#[derive(PartialEq)]
struct BrowserSpot(PathBuf, bool, Option<PathBuf>);

/// The `~` key's check of the startup home, and where the browser was when `~` was pressed.
pub(super) struct HomeCheck {
    id: u64,
    from: BrowserSpot,
}

/// A `folder-check` worker's answer.
pub(super) enum FolderCheckAnswer {
    /// `~`: whether the startup home is a folder.
    Home { id: u64, dir: PathBuf, is_dir: bool },
    /// Mix picker / Seed Radio: the folder Autopilot scans.
    Autopilot { id: u64, dir: PathBuf },
}

impl AppUI {
    fn browser_spot(&self) -> BrowserSpot {
        BrowserSpot(
            self.browser_dir.clone(),
            self.in_bookmarks_view,
            self.opening_folder().map(Path::to_path_buf),
        )
    }

    fn next_folder_check_id(&mut self) -> u64 {
        self.folder_check_id += 1;
        self.folder_check_id
    }

    /// `~`: open `home` once a worker has found it is a folder (`Jumped to home: …`), else say
    /// `Home directory not found: …`. If the browser has moved on by then, it stays where it is.
    pub(super) fn jump_to_home(&mut self, home: PathBuf) {
        let id = self.next_folder_check_id();
        self.home_check = Some(HomeCheck {
            id,
            from: self.browser_spot(),
        });
        let check = self.path_check.clone();
        let tx = self.folder_check_tx.clone();
        self.workers.spawn("folder-check", move || {
            let is_dir = check(&home) == PathKind::Dir;
            let _ = tx.send(FolderCheckAnswer::Home { id, dir: home, is_dir });
        });
    }

    /// Start Autopilot in `preferred` once a worker has found it exists, else in `fallback` (the
    /// current directory when `None`). Any other Autopilot start, or the end of the playback
    /// session, before the answer drops it.
    pub(super) fn start_autopilot_after_check(&mut self, preferred: Option<PathBuf>, fallback: Option<PathBuf>) {
        let id = self.next_folder_check_id();
        self.autopilot_check = Some(id);
        let check = self.path_check.clone();
        let tx = self.folder_check_tx.clone();
        self.workers.spawn("folder-check", move || {
            let dir = match preferred {
                Some(dir) if check(&dir) != PathKind::Missing => dir,
                _ => fallback.unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            };
            let _ = tx.send(FolderCheckAnswer::Autopilot { id, dir });
        });
    }

    /// Whether a key's folder check is still waiting for its answer (tests).
    #[doc(hidden)]
    pub fn folder_check_pending(&self) -> bool {
        self.home_check.is_some() || self.autopilot_check.is_some()
    }

    /// How many folder check answers have been taken in, dropped ones included (tests).
    #[doc(hidden)]
    pub fn folder_check_answers(&self) -> u64 {
        self.folder_check_answers
    }

    /// Act on the folder checks that have answered. Never blocks; the event loop calls it once
    /// per tick.
    pub fn poll_folder_checks(&mut self) {
        while let Ok(answer) = self.folder_check_rx.try_recv() {
            self.folder_check_answers += 1;
            match answer {
                FolderCheckAnswer::Home { id, dir, is_dir } => {
                    let check = match self.home_check.take() {
                        Some(check) if check.id == id => check,
                        newer => {
                            self.home_check = newer;
                            continue;
                        }
                    };
                    if check.from != self.browser_spot() {
                        continue;
                    }
                    if is_dir {
                        self.status_message = format!("Jumped to home: {}", dir.display());
                        self.navigate_browser(dir);
                    } else {
                        self.status_message = format!("Home directory not found: {}", dir.display());
                    }
                }
                FolderCheckAnswer::Autopilot { id, dir } => {
                    if self.autopilot_check == Some(id) {
                        self.autopilot_check = None;
                        self.start_autopilot(dir);
                    }
                }
            }
        }
    }
}
