//! The background library indexer as the UI runs it (#41): which folders it walks
//! ([`index_roots`], from memory), starting it from `run()` and when the Library Index setting is
//! turned on, stopping it when the setting is turned off and on shutdown, and once per tick
//! (`poll_library_index`) pausing it while the UI reads the library itself (a mix gathering, a
//! folder being read, a search scanning) and restarting it when its folders change.
//!
//! At most one indexer thread runs. A stopped thread can be stuck in a stat on a hung share for
//! as long as the share hangs, so a new one starts only once the old one has finished
//! (`restart_pending`): threads never pile up. The UI thread only writes the indexer's flags and
//! reads its progress atomics; the indexer opens its own database connection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::AppUI;
use crate::analysis::index::{
    default_tag_reader, run_library_index, IndexConfig, IndexControl, IndexProgress, IndexState, IndexTagReader,
};
use crate::analysis::AnalysisDatabase;

/// Opens the indexer's own connection to the analysis database.
pub type IndexDbOpener = Arc<dyn Fn() -> anyhow::Result<AnalysisDatabase> + Send + Sync>;

/// What the indexer runs with in place of the app's database, tag reads and pacing (tests).
/// `config.roots` is ignored: the roots are always [`AppUI::index_roots`].
#[derive(Clone)]
pub struct LibraryIndexSource {
    pub open_db: IndexDbOpener,
    pub reader: IndexTagReader,
    pub config: IndexConfig,
}

/// The running (or last) indexer thread.
pub struct LibraryIndexHandle {
    control: Arc<IndexControl>,
    /// Set when the thread has returned (or unwound).
    finished: Arc<AtomicBool>,
    /// The folders it walks.
    roots: Vec<PathBuf>,
}

impl LibraryIndexHandle {
    fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Running and not asked to stop.
    fn live(&self) -> bool {
        !self.finished() && !self.control.is_stopped()
    }
}

/// Sets its flag when dropped: when the thread's closure returns or unwinds.
struct FinishedGuard(Arc<AtomicBool>);

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The indexer's state in `AppUI`.
pub struct LibraryIndexUi {
    handle: Option<LibraryIndexHandle>,
    /// Start a new thread once the old one has finished (setting turned on again, folders
    /// changed).
    restart_pending: bool,
    /// One for the app's lifetime: a new thread starts only after the old one has finished.
    progress: Arc<IndexProgress>,
    source: Option<LibraryIndexSource>,
    starts: usize,
}

impl LibraryIndexUi {
    pub fn new() -> Self {
        Self {
            handle: None,
            restart_pending: false,
            progress: Arc::new(IndexProgress::new()),
            source: None,
            starts: 0,
        }
    }
}

impl Default for LibraryIndexUi {
    fn default() -> Self {
        Self::new()
    }
}

/// The folders the indexer walks: the startup home (`home_dir`), then the Music Homes, each
/// normalized lexically and kept only when the web dashboard could have it as an allowed folder
/// (`web_access::is_acceptable_root`: absolute, not `/`, not `home` or a folder above it), each
/// once, and none that lies inside another. No disk access.
pub fn index_roots(home_dir: Option<&str>, bookmarks: &[String], home: Option<&Path>) -> Vec<PathBuf> {
    let candidates: Vec<PathBuf> = home_dir
        .into_iter()
        .chain(bookmarks.iter().map(String::as_str))
        .map(|p| crate::playlist::normalize_path(Path::new(p)))
        .filter(|p| crate::web_access::is_acceptable_root(p, home))
        .collect();
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in &candidates {
        let nested = candidates.iter().any(|other| other != root && root.starts_with(other));
        if !nested && !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    roots
}

impl AppUI {
    /// The folders the indexer walks ([`index_roots`] of the startup home and Music Homes in
    /// memory).
    pub fn index_roots(&self) -> Vec<PathBuf> {
        index_roots(
            self.settings_mgr.settings.home_dir.as_deref(),
            &self.bookmarks,
            self.queue_tab.home.as_deref(),
        )
    }

    /// The indexer's progress, read by the Settings tab (tests set it to show given values).
    pub fn library_index_progress(&self) -> Arc<IndexProgress> {
        self.library_index.progress.clone()
    }

    /// Run the indexer with `source` instead of the app's database and tag reads (test seam).
    /// Takes effect at the next start.
    #[doc(hidden)]
    pub fn set_library_index_source(&mut self, source: LibraryIndexSource) {
        self.library_index.source = Some(source);
    }

    /// How many indexer threads this app has started (tests).
    #[doc(hidden)]
    pub fn library_index_starts(&self) -> usize {
        self.library_index.starts
    }

    /// Whether an indexer thread is running and not asked to stop.
    pub fn library_index_running(&self) -> bool {
        self.library_index.handle.as_ref().is_some_and(LibraryIndexHandle::live)
    }

    /// Start the indexer if the Library Index setting is on and none runs. A thread asked to stop
    /// that is still running (stuck on a hung share, say) is waited for: the new one starts from
    /// `poll_library_index` once it has finished.
    pub fn start_library_index(&mut self) {
        if !self.settings_mgr.settings.library_index {
            self.stop_library_index();
            return;
        }
        if let Some(handle) = &self.library_index.handle {
            if handle.live() {
                return;
            }
            if !handle.finished() {
                self.library_index.restart_pending = true;
                return;
            }
        }
        self.library_index.restart_pending = false;
        let roots = self.index_roots();
        let source = match &self.library_index.source {
            Some(source) => source.clone(),
            None => {
                let path = self.analysis_db.path();
                LibraryIndexSource {
                    open_db: Arc::new(move || match &path {
                        Some(path) => AnalysisDatabase::open_at(path),
                        None => Err(anyhow::anyhow!("the analysis database is in memory")),
                    }),
                    reader: default_tag_reader(),
                    config: IndexConfig::default(),
                }
            }
        };
        let cfg = IndexConfig {
            roots: roots.clone(),
            ..source.config.clone()
        };
        let control = Arc::new(IndexControl::new());
        let finished = Arc::new(AtomicBool::new(false));
        let progress = self.library_index.progress.clone();
        let (thread_control, guard) = (control.clone(), FinishedGuard(finished.clone()));
        crate::log_info!("index", "Starting the library index over {} folder(s)", roots.len());
        self.workers.spawn("library-index", move || {
            let _guard = guard;
            let open_db = source.open_db;
            run_library_index(|| open_db(), cfg, &thread_control, &progress, source.reader);
        });
        self.library_index.handle = Some(LibraryIndexHandle {
            control,
            finished,
            roots,
        });
        self.library_index.starts += 1;
    }

    /// Ask the indexer to stop (setting off, shutdown). It finishes on its own.
    pub fn stop_library_index(&mut self) {
        self.library_index.restart_pending = false;
        if let Some(handle) = &self.library_index.handle {
            handle.control.stop();
        }
    }

    /// Once per tick: hold the indexer while the UI reads the library itself (a mix gathering, a
    /// folder being read, the search scanning), restart it when the folders it walks changed (a
    /// Music Home or the startup home), and start the thread a restart waits for once the old
    /// one has finished. Memory only.
    pub fn poll_library_index(&mut self) {
        let Some(handle) = &self.library_index.handle else {
            return;
        };
        let busy = self.is_autopilot_scanning
            || self.browser_loading
            || self.opening_folder().is_some()
            || self.search_modal.as_ref().is_some_and(|modal| modal.is_scanning);
        handle.control.set_paused(busy);
        let stopped_by_us = handle.control.is_stopped() && !self.library_index.restart_pending;
        if self.settings_mgr.settings.library_index && !stopped_by_us && handle.roots != self.index_roots() {
            handle.control.stop();
            self.library_index.restart_pending = true;
        }
        if self.library_index.restart_pending && handle.finished() {
            self.start_library_index();
        }
    }

    /// The indexer's control while it runs, with its roots: the prioritise hint a mix sends.
    pub(super) fn library_index_hint(&self) -> Option<(Arc<IndexControl>, Vec<PathBuf>)> {
        let handle = self.library_index.handle.as_ref().filter(|h| h.live())?;
        Some((handle.control.clone(), handle.roots.clone()))
    }

    /// After the Library Index setting changed: start or stop the indexer.
    pub(super) fn apply_library_index_setting(&mut self) {
        if self.settings_mgr.settings.library_index {
            self.start_library_index();
        } else {
            self.stop_library_index();
            self.library_index.progress.set_state(IndexState::Off);
        }
    }
}
