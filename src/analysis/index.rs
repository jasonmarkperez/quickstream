//! The background library index (#41): walks the Music Homes and the startup home on one
//! thread, reads the tags of every audio file whose stamp (mtime, size) has no row yet, and
//! stores them as tag-only rows in the analysis database (`write_index_batch`), with one
//! `library_index_dirs` record per folder. Profile mixes then draw indexed folders from their
//! rows instead of walking them (`library.rs`, index first).
//!
//! It only reads the library: `read_dir`, one stat per audio file and a tag read for a file that
//! changed. It writes nothing but its own connection to the analysis database, and never holds a
//! transaction across file I/O (rows are read first, then written in batches of `write_every`).
//!
//! The UI talks to it through atomics and one mutex only ([`IndexControl`], [`IndexProgress`]):
//! `stop`, `paused` (while a mix gathers, a folder is read or a search scans) and a folder to
//! walk next (`prioritise`, the folder a mix started in). A slow operation (a share that stalls)
//! makes it back off; every wait ends early when `stop` is raised.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::db::{mtime_nanos, walk_skips, AnalysisDatabase, IndexedDir, TrackFeatures};
use super::worker::{stamp_of, tag_features, TagReadError};
use crate::library::SeenPaths;
use crate::metadata::is_supported_audio_file;

/// How the indexer walks and paces itself.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    /// The folders it walks (each with everything under it), in order.
    pub roots: Vec<PathBuf>,
    /// How many folder levels under a root it walks (a root's own files are level 0).
    pub max_depth: usize,
    /// Rows written per transaction.
    pub write_every: usize,
    /// A listing, stat or tag read that takes this long counts as slow.
    pub slow_op: Duration,
    /// How long it waits after a slow operation before the next one.
    pub backoff: Duration,
    /// How long it waits after a pass before walking everything again.
    pub rescan_interval: Duration,
    /// How long it waits before its first pass (startup stays quiet).
    pub start_delay: Duration,
}

impl IndexConfig {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            max_depth: 16,
            write_every: 32,
            slow_op: Duration::from_secs(2),
            backoff: Duration::from_secs(30),
            rescan_interval: Duration::from_secs(6 * 60 * 60),
            start_delay: Duration::from_secs(10),
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// What the UI tells the indexer.
#[derive(Debug, Default)]
pub struct IndexControl {
    stop: AtomicBool,
    paused: AtomicBool,
    prioritise: Mutex<Option<PathBuf>>,
}

impl IndexControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the indexer to stop; every wait it is in ends early.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Hold the indexer (between two files) while `paused`.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Walk `dir` next (the folder a mix started in), if it lies under a root and this pass has
    /// not walked it yet. A later call replaces an earlier one not yet taken.
    pub fn prioritise(&self, dir: PathBuf) {
        *self.prioritise.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
    }

    fn take_prioritised(&self) -> Option<PathBuf> {
        self.prioritise.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Where the indexer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexState {
    /// Not started yet (the start delay).
    Waiting = 0,
    Reading = 1,
    /// Held while the UI reads the library itself.
    Paused = 2,
    /// A pass finished; the next starts after `rescan_interval`.
    Done = 3,
    /// Turned off in the settings (set by the UI).
    Off = 4,
    /// No folder to index (no Music Home or startup home it may walk).
    NoFolders = 5,
    /// The analysis database couldn't be opened.
    Unavailable = 6,
}

impl IndexState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => IndexState::Reading,
            2 => IndexState::Paused,
            3 => IndexState::Done,
            4 => IndexState::Off,
            5 => IndexState::NoFolders,
            6 => IndexState::Unavailable,
            _ => IndexState::Waiting,
        }
    }
}

/// What the indexer has done in its current (or last) pass, as atomics the UI reads per frame.
#[derive(Debug, Default)]
pub struct IndexProgress {
    state: AtomicU8,
    /// Top-level folders (a root's direct subfolders) walked whole this pass.
    pub top_done: AtomicUsize,
    /// Top-level folders under every root; 0 until the roots are listed.
    pub top_total: AtomicUsize,
    /// Files whose tags were read this pass.
    pub read: AtomicUsize,
    /// Files whose row already matched their stamp (no tag read).
    pub unchanged: AtomicUsize,
    /// Files that couldn't be stat'ed or read (retried next pass).
    pub failed: AtomicUsize,
    /// Slow operations it backed off after.
    pub backoffs: AtomicUsize,
}

/// A copy of [`IndexProgress`] at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSnapshot {
    pub state: IndexState,
    pub top_done: usize,
    pub top_total: usize,
    pub read: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub backoffs: usize,
}

impl IndexProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> IndexState {
        IndexState::from_u8(self.state.load(Ordering::SeqCst))
    }

    pub fn set_state(&self, state: IndexState) {
        self.state.store(state as u8, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> IndexSnapshot {
        let get = |a: &AtomicUsize| a.load(Ordering::SeqCst);
        IndexSnapshot {
            state: self.state(),
            top_done: get(&self.top_done),
            top_total: get(&self.top_total),
            read: get(&self.read),
            unchanged: get(&self.unchanged),
            failed: get(&self.failed),
            backoffs: get(&self.backoffs),
        }
    }

    /// Zero the counters a pass counts (the state is left as it is).
    fn reset_pass(&self) {
        for counter in [
            &self.top_done,
            &self.top_total,
            &self.read,
            &self.unchanged,
            &self.failed,
            &self.backoffs,
        ] {
            counter.store(0, Ordering::SeqCst);
        }
    }
}

/// How the indexer reads one file's tags for its stamp: [`tag_features`] except in tests, which
/// count reads, fail them or hold them back like a hung share.
pub type IndexTagReader = Arc<dyn Fn(&Path, (u64, u64)) -> Result<TrackFeatures, TagReadError> + Send + Sync>;

/// The reader the app uses: [`tag_features`].
pub fn default_tag_reader() -> IndexTagReader {
    Arc::new(|path: &Path, stamp: (u64, u64)| tag_features(path, stamp))
}

/// Walk `cfg.roots` again and again (a pass, then `rescan_interval`) until `control` is stopped,
/// after `cfg.start_delay`. `open_db` opens the indexer's own connection; when it fails the state
/// is [`IndexState::Unavailable`] and it returns. With no roots it reports
/// [`IndexState::NoFolders`] and returns.
pub fn run_library_index(
    open_db: impl FnOnce() -> anyhow::Result<AnalysisDatabase>,
    cfg: IndexConfig,
    control: &IndexControl,
    progress: &IndexProgress,
    reader: IndexTagReader,
) {
    if cfg.roots.is_empty() {
        progress.set_state(IndexState::NoFolders);
        return;
    }
    progress.set_state(IndexState::Waiting);
    if !wait(control, cfg.start_delay) {
        return;
    }
    let db = match open_db() {
        Ok(db) => db,
        Err(e) => {
            crate::log_warn!("index", "Library index unavailable: couldn't open the database ({e})");
            progress.set_state(IndexState::Unavailable);
            return;
        }
    };
    let mut indexer = Indexer {
        db,
        cfg,
        control,
        progress,
        reader,
        walked: HashMap::new(),
    };
    loop {
        let start = Instant::now();
        let finished = indexer.pass();
        let p = progress.snapshot();
        crate::log_info!(
            "index",
            "Library index pass {} in {:.1?}: {}/{} folders, {} read, {} unchanged, {} failed, {} backoffs",
            if finished { "done" } else { "stopped" },
            start.elapsed(),
            p.top_done,
            p.top_total,
            p.read,
            p.unchanged,
            p.failed,
            p.backoffs
        );
        if !finished || control.is_stopped() {
            return;
        }
        progress.set_state(IndexState::Done);
        if !wait(control, indexer.cfg.rescan_interval) {
            return;
        }
    }
}

/// Sleep `d` in short steps, ending early when `control` is stopped. Returns false if stopped.
fn wait(control: &IndexControl, d: Duration) -> bool {
    let until = Instant::now() + d;
    while !control.is_stopped() {
        let now = Instant::now();
        if now >= until {
            return true;
        }
        std::thread::sleep((until - now).min(Duration::from_millis(20)));
    }
    false
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A folder's own mtime as the index records it with a subtree mark ([`mtime_nanos`]): what a
/// mix compares with the recorded one (`!=`) to tell whether the folder changed since its
/// subtree was walked. `None` when it can't be read.
pub fn folder_mtime(dir: &Path) -> Option<i64> {
    std::fs::metadata(dir).ok()?.modified().ok().and_then(mtime_nanos)
}

/// A folder's audio files and subfolders, as the walk takes them.
#[derive(Default)]
struct Listing {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

/// One root of a pass: whether its own files and every top-level folder under it came out whole,
/// and its mtime when it was listed (the subtree mark is recorded for it).
struct RootPlan {
    root: PathBuf,
    whole: bool,
    mtime: Option<i64>,
}

struct Indexer<'a> {
    db: AnalysisDatabase,
    cfg: IndexConfig,
    control: &'a IndexControl,
    progress: &'a IndexProgress,
    reader: IndexTagReader,
    /// Folders a prioritised walk did whole or not this pass, reused when the walk reaches them.
    walked: HashMap<PathBuf, bool>,
}

impl Indexer<'_> {
    /// Hold while paused. Returns false once stopped.
    fn hold(&self) -> bool {
        if self.control.is_paused() {
            self.progress.set_state(IndexState::Paused);
            while self.control.is_paused() && !self.control.is_stopped() {
                std::thread::sleep(Duration::from_millis(20));
            }
            if self.control.is_stopped() {
                // Stopped while paused: the state is the UI's now (`Off`, or a new thread's).
                return false;
            }
            self.progress.set_state(IndexState::Reading);
        }
        !self.control.is_stopped()
    }

    /// Run `op`; if it was slow (a stalled share), back off before going on.
    fn timed<T>(&self, what: &str, path: &Path, op: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = op();
        let took = start.elapsed();
        if took >= self.cfg.slow_op {
            self.progress.backoffs.fetch_add(1, Ordering::SeqCst);
            crate::log_info!(
                "index",
                "Slow {what} of '{}' ({took:.1?}); waiting {:?}",
                path.display(),
                self.cfg.backoff
            );
            wait(self.control, self.cfg.backoff);
        }
        out
    }

    /// The folder's audio files and subfolders, with the scanner's filter: names the walks skip
    /// (`db::walk_skips`), names that are not UTF-8, symlinks, duplicate entries (#35), files
    /// that are not audio.
    fn list(&self, dir: &Path) -> std::io::Result<Listing> {
        self.timed("listing", dir, || {
            let mut listing = Listing::default();
            let mut seen = SeenPaths::default();
            for entry in std::fs::read_dir(dir)? {
                if self.control.is_stopped() {
                    break;
                }
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if walk_skips(name) {
                    continue;
                }
                let path = entry.path();
                if !seen.first_visit(&path) {
                    continue;
                }
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    listing.dirs.push(path);
                } else if kind.is_file() && is_supported_audio_file(&path) {
                    listing.files.push(path);
                }
            }
            listing.files.sort();
            listing.dirs.sort();
            Ok(listing)
        })
    }

    /// Write `rows` and (with `dir`) the folder's record. Returns whether it worked.
    fn write(&self, rows: &[TrackFeatures], dir: Option<&IndexedDir>) -> bool {
        match self.db.write_index_batch(rows, dir) {
            Ok(_) => true,
            Err(e) => {
                crate::log_warn!("index", "Couldn't write {} index rows: {e}", rows.len());
                false
            }
        }
    }

    /// Read the tags of `files` (a folder's own audio files) that changed and record the folder.
    /// `listed_ok` is false when the listing failed (the folder can't be complete). Returns
    /// whether every file has a current row, `None` when stopped part-way (the rows read so far
    /// are kept and the folder recorded as incomplete).
    fn index_files(&self, dir: &Path, files: &[PathBuf], listed_at: u64, listed_ok: bool) -> Option<bool> {
        let record = |complete: bool| IndexedDir {
            dirpath: dir.to_path_buf(),
            audio_files: files.len(),
            complete,
            walked_at: listed_at,
        };
        let mut complete = listed_ok;
        let mut stamped = Vec::with_capacity(files.len());
        for path in files {
            if !self.hold() {
                self.write(&[], Some(&record(false)));
                return None;
            }
            match self.timed("stat", path, || std::fs::metadata(path)) {
                Ok(md) if md.is_file() => stamped.push((path.clone(), stamp_of(&md))),
                Ok(_) => {}
                // Gone since the listing: nothing to index.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    crate::log_info!("index", "Couldn't stat '{}': {e}", path.display());
                    self.progress.failed.fetch_add(1, Ordering::SeqCst);
                    complete = false;
                }
            }
        }
        let paths: Vec<PathBuf> = stamped.iter().map(|(p, _)| p.clone()).collect();
        let known = self.db.index_stamps(&paths);
        let mut batch = Vec::new();
        for (path, stamp) in stamped {
            if !self.hold() {
                self.write(&batch, Some(&record(false)));
                return None;
            }
            if known.get(path.to_string_lossy().as_ref()) == Some(&stamp) {
                self.progress.unchanged.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            match self.timed("tag read", &path, || (self.reader)(&path, stamp)) {
                Ok(row) => {
                    self.progress.read.fetch_add(1, Ordering::SeqCst);
                    batch.push(row);
                }
                Err(e) => {
                    crate::log_info!("index", "Couldn't read the tags of '{}': {e}", path.display());
                    self.progress.failed.fetch_add(1, Ordering::SeqCst);
                    complete = false;
                }
            }
            if batch.len() >= self.cfg.write_every.max(1) {
                complete &= self.write(&batch, None);
                batch.clear();
            }
        }
        if self.control.is_stopped() {
            self.write(&batch, Some(&record(false)));
            return None;
        }
        complete &= self.write(&batch, Some(&record(complete)));
        Some(complete)
    }

    /// List `dir`, index its files and record it. Returns the listing (empty when it failed),
    /// whether its own files are whole and the folder's mtime as seen just before the listing
    /// (what its subtree mark is recorded for), or `None` when stopped.
    fn index_dir(&self, dir: &Path) -> Option<(Listing, bool, Option<i64>)> {
        if !self.hold() {
            return None;
        }
        // The folder's mtime first, then its listing: an entry added after the stat changes the
        // mtime again, so the recorded one never vouches for an entry the listing missed.
        let mtime = self.timed("stat", dir, || folder_mtime(dir));
        let listed_at = now_secs();
        let (listing, ok) = match self.list(dir) {
            Ok(listing) => (listing, true),
            Err(e) => {
                crate::log_info!("index", "Couldn't list '{}': {e}", dir.display());
                (Listing::default(), false)
            }
        };
        if self.control.is_stopped() {
            return None;
        }
        let own = self.index_files(dir, &listing.files, listed_at, ok)?;
        Some((listing, own && mtime.is_some(), mtime))
    }

    /// Record whether `dir` and everything under it are indexed: whole as of the folder mtime
    /// `mtime` (seen when this walk listed it), or not.
    fn set_subtree(&self, dir: &Path, whole: bool, mtime: Option<i64>) {
        let whole_at = if whole { mtime } else { None };
        if let Err(e) = self.db.set_subtree_complete(dir, whole_at) {
            crate::log_warn!("index", "Couldn't record '{}': {e}", dir.display());
        }
    }

    /// Index `dir` (at `depth` under its root) and every folder under it, depth-first. Returns
    /// whether all of it is indexed, `None` when stopped.
    fn walk_tree(&mut self, dir: &Path, depth: usize) -> Option<bool> {
        if let Some(&whole) = self.walked.get(dir) {
            return Some(whole);
        }
        let (listing, own, mtime) = self.index_dir(dir)?;
        let mut whole = own;
        if !listing.dirs.is_empty() && depth >= self.cfg.max_depth {
            // Too deep to walk: what is under it is not indexed.
            whole = false;
        } else {
            for sub in &listing.dirs {
                whole &= self.walk_tree(sub, depth + 1)?;
            }
        }
        self.set_subtree(dir, whole, mtime);
        Some(whole)
    }

    /// One pass over every root. Returns false when stopped part-way.
    fn pass(&mut self) -> bool {
        self.progress.reset_pass();
        self.progress.set_state(IndexState::Reading);
        self.walked.clear();
        let mut rng = crate::queue::FastRng::new();

        // List every root first (its own files indexed), so the folder total is known.
        let mut plans = Vec::new();
        let mut queue: VecDeque<(usize, PathBuf)> = VecDeque::new();
        for root in self.cfg.roots.clone() {
            let Some((mut listing, own, mtime)) = self.index_dir(&root) else {
                return false;
            };
            rng.shuffle(&mut listing.dirs);
            self.progress.top_total.fetch_add(listing.dirs.len(), Ordering::SeqCst);
            let at = plans.len();
            plans.push(RootPlan {
                root,
                whole: own,
                mtime,
            });
            queue.extend(listing.dirs.into_iter().map(|dir| (at, dir)));
        }

        // Then each root's top-level folders, a prioritised one first.
        loop {
            if let Some(dir) = self.control.take_prioritised() {
                if !self.prioritise(&dir, &mut queue, &plans) {
                    return false;
                }
            }
            let Some((at, dir)) = queue.pop_front() else {
                break;
            };
            let Some(whole) = self.walk_tree(&dir, 1) else {
                return false;
            };
            plans[at].whole &= whole;
            self.progress.top_done.fetch_add(1, Ordering::SeqCst);
        }
        for plan in &plans {
            self.set_subtree(&plan.root, plan.whole, plan.mtime);
        }
        true
    }

    /// Put `dir` first: a top-level folder still to walk moves to the front, a folder deeper
    /// under one is walked now (and reused when the walk gets there), a root's folders move to
    /// the front. A folder under no root, or already walked, is left. Returns false if stopped.
    fn prioritise(&mut self, dir: &Path, queue: &mut VecDeque<(usize, PathBuf)>, plans: &[RootPlan]) -> bool {
        if let Some(pos) = queue.iter().position(|(_, d)| d == dir) {
            if let Some(item) = queue.remove(pos) {
                queue.push_front(item);
            }
            return true;
        }
        if let Some((_, top)) = queue.iter().find(|(_, d)| dir.starts_with(d)) {
            let depth = 1 + dir.components().count() - top.components().count();
            if depth > self.cfg.max_depth {
                return true;
            }
            crate::log_info!("index", "Indexing '{}' first", dir.display());
            let Some(whole) = self.walk_tree(dir, depth) else {
                return false;
            };
            self.walked.insert(dir.to_path_buf(), whole);
            return true;
        }
        if let Some(at) = plans.iter().position(|p| p.root == dir) {
            let (mine, rest): (Vec<_>, Vec<_>) = queue.drain(..).partition(|(i, _)| *i == at);
            queue.extend(mine);
            queue.extend(rest);
        }
        true
    }
}
