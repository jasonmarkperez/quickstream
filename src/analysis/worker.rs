//! Background track analysis worker.
//!
//! Fills `track_features` for tracks discovered by the library scanner / autopilot so that
//! `DjMixer::sequence_mix` and the BPM / mood profiles have real BPM, key and energy data.
//!
//! - Embedded tags first (BPM, initial key) via lofty; ffmpeg acoustic analysis only when the
//!   BPM tag is missing.
//! - Resumable: tracks whose row already matches the file's mtime and size are skipped.
//! - Bounded concurrency (a small fixed pool of threads), never on the UI thread.
//! - Two queues ([`AnalysisQueue`]): tracks that reached the playback queue go first; an
//!   autopilot run's candidate pool (the tracks it gathered but did not queue, #30) waits behind
//!   them, at most [`MAX_PENDING_CANDIDATES`] (the oldest give way), so a 200-track pool delays
//!   a queued track by at most the one candidate being analysed.
//! - Read-only towards the music library: the only writes go to the analysis database.
//! - Stops promptly: the stop flag is checked between tracks and while ffmpeg runs.
//! - A panic while a track is analysed costs that track only (counted as failed).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::bpm::{BpmAnalyzer, FfmpegConfig};
use super::db::{AnalysisDatabase, TrackFeatures};
use super::harmonic::MusicalKey;

/// Snapshot of worker progress counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalysisProgress {
    /// Tracks analysed and written to the database this session.
    pub analyzed: usize,
    /// Tracks skipped because the database already had an up-to-date row.
    pub skipped: usize,
    /// Tracks whose acoustic analysis failed (ffmpeg missing / error / timeout / no audio) or
    /// panicked. Nothing is stored for them, so they are retried in a later session.
    pub failed: usize,
    /// Tracks queued but not yet processed.
    pub pending: usize,
}

/// At most this many autopilot candidates wait for analysis (one run's pool); a later run's
/// candidates push out the oldest.
pub const MAX_PENDING_CANDIDATES: usize = 200;

/// The tracks waiting for analysis: tracks that reached the playback queue first, in the order
/// they came, then autopilot candidates (bounded, the oldest dropped first). A path is taken at
/// most once a session: offered again while it waits, while it is analysed or after, it is
/// ignored, except that a waiting candidate that reaches the queue moves to the queued tracks. A
/// dropped candidate is forgotten, so a later run may offer it again.
#[derive(Debug)]
pub struct AnalysisQueue {
    queued: VecDeque<PathBuf>,
    candidates: VecDeque<PathBuf>,
    candidate_limit: usize,
    seen: HashSet<PathBuf>,
}

impl AnalysisQueue {
    /// An empty queue holding at most `candidate_limit` candidates.
    pub fn new(candidate_limit: usize) -> Self {
        Self {
            queued: VecDeque::new(),
            candidates: VecDeque::new(),
            candidate_limit,
            seen: HashSet::new(),
        }
    }

    /// Add a track that reached the playback queue. Returns whether it is new work (a waiting
    /// candidate moved ahead is not).
    pub fn push_queued(&mut self, path: PathBuf) -> bool {
        if self.seen.insert(path.clone()) {
            self.queued.push_back(path);
            return true;
        }
        if let Some(at) = self.candidates.iter().position(|c| *c == path) {
            self.candidates.remove(at);
            self.queued.push_back(path);
        }
        false
    }

    /// Add an autopilot candidate behind the queued tracks, dropping the oldest candidate when
    /// the bound is reached. Returns whether it was added.
    pub fn push_candidate(&mut self, path: PathBuf) -> bool {
        if self.candidate_limit == 0 || self.seen.contains(&path) {
            return false;
        }
        while self.candidates.len() >= self.candidate_limit {
            if let Some(oldest) = self.candidates.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(path.clone());
        self.candidates.push_back(path);
        true
    }

    /// Add one autopilot run's candidates, as [`Self::push_candidate`] does each, up to the bound:
    /// older runs' candidates give way, but once the run has filled the bound its later paths are
    /// left out, so it never pushes out its own first ones. Returns whether any was added.
    pub fn push_candidates<I: IntoIterator<Item = PathBuf>>(&mut self, paths: I) -> bool {
        let mut added = 0;
        for path in paths {
            if added == self.candidate_limit {
                break;
            }
            if self.push_candidate(path) {
                added += 1;
            }
        }
        added > 0
    }

    /// The next track to analyse: the oldest queued track, else the oldest candidate.
    pub fn pop(&mut self) -> Option<PathBuf> {
        self.queued.pop_front().or_else(|| self.candidates.pop_front())
    }

    /// Tracks waiting, queued and candidates.
    pub fn len(&self) -> usize {
        self.queued.len() + self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The queue and the threads' bookkeeping, behind one lock that is never held while a track is
/// analysed (so `enqueue` on the UI thread never waits for an analysis).
struct Work {
    queue: AnalysisQueue,
    /// Tracks being analysed right now.
    in_flight: usize,
}

struct Shared {
    /// Raised on shutdown, with the `work` lock held so a thread about to wait sees it or is
    /// woken: nothing more is queued or taken, and a running ffmpeg call is aborted.
    stop: AtomicBool,
    analyzed: AtomicUsize,
    skipped: AtomicUsize,
    failed: AtomicUsize,
    /// Waiting plus in flight: written under the `work` lock, read without it.
    pending: AtomicUsize,
    work: Mutex<Work>,
    /// Signalled when work is queued and on shutdown (after the change, made under `work`).
    ready: Condvar,
    analyse: Analyser,
}

impl Shared {
    fn work(&self) -> MutexGuard<'_, Work> {
        self.work.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Republish the pending count after a change to `work` (its lock held).
    fn publish_pending(&self, work: &Work) {
        self.pending.store(work.queue.len() + work.in_flight, Ordering::SeqCst);
    }
}

/// What the worker runs on each track that needs analysing: [`analyze_track`] with the worker's
/// ffmpeg settings, or a stand-in (test seam). The flag is raised on shutdown.
pub type Analyser = Arc<dyn Fn(&Path, &AtomicBool) -> Result<TrackFeatures, AnalysisError> + Send + Sync>;

/// Handle to the background analysis thread pool. Dropping it stops and joins the threads.
pub struct AnalysisWorker {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl AnalysisWorker {
    /// Spawn `threads` (clamped to 1..=2) analysis threads writing into `db`.
    pub fn spawn(db: AnalysisDatabase, threads: usize) -> Self {
        Self::spawn_with_ffmpeg(db, threads, FfmpegConfig::default())
    }

    /// Like [`Self::spawn`], but with an explicit ffmpeg binary / timeout for acoustic analysis.
    pub fn spawn_with_ffmpeg(db: AnalysisDatabase, threads: usize, ffmpeg: FfmpegConfig) -> Self {
        let analyse: Analyser = Arc::new(move |path, stop| analyze_track_with(&ffmpeg, path, Some(stop)));
        Self::spawn_with_analyser(db, threads, analyse)
    }

    /// Like [`Self::spawn`], but running `analyse` on each track that needs analysing (test seam).
    pub fn spawn_with_analyser(db: AnalysisDatabase, threads: usize, analyse: Analyser) -> Self {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            analyzed: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            work: Mutex::new(Work {
                queue: AnalysisQueue::new(MAX_PENDING_CANDIDATES),
                in_flight: 0,
            }),
            ready: Condvar::new(),
            analyse,
        });

        let handles = (0..threads.clamp(1, 2))
            .filter_map(|i| {
                let shared = Arc::clone(&shared);
                let db = db.clone();
                std::thread::Builder::new()
                    .name(format!("qs-analysis-{}", i))
                    .spawn(move || worker_loop(shared, db))
                    .ok()
            })
            .collect();

        Self { shared, handles }
    }

    /// Queue tracks that reached the playback queue: they are analysed before any candidate.
    /// Paths already queued this session are ignored (a waiting candidate moves ahead).
    pub fn enqueue<I: IntoIterator<Item = PathBuf>>(&self, paths: I) {
        self.push(|queue| {
            let mut added = false;
            for path in paths {
                added |= queue.push_queued(path);
            }
            added
        });
    }

    /// Queue an autopilot run's candidate pool behind the queued tracks, at most
    /// [`MAX_PENDING_CANDIDATES`] waiting: older runs' candidates give way, and past the bound the
    /// pool's later paths are left out (see [`AnalysisQueue::push_candidates`]). Paths already
    /// queued this session are ignored.
    pub fn enqueue_candidates<I: IntoIterator<Item = PathBuf>>(&self, paths: I) {
        self.push(|queue| queue.push_candidates(paths));
    }

    /// Run `add` on the queue (it returns whether anything was added) and wake a thread for it.
    fn push(&self, add: impl FnOnce(&mut AnalysisQueue) -> bool) {
        let mut work = self.shared.work();
        if self.shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let added = add(&mut work.queue);
        self.shared.publish_pending(&work);
        drop(work);
        if added {
            self.shared.ready.notify_all();
        }
    }

    /// Current progress counters (lock-free).
    pub fn progress(&self) -> AnalysisProgress {
        AnalysisProgress {
            analyzed: self.shared.analyzed.load(Ordering::SeqCst),
            skipped: self.shared.skipped.load(Ordering::SeqCst),
            failed: self.shared.failed.load(Ordering::SeqCst),
            pending: self.shared.pending.load(Ordering::SeqCst),
        }
    }

    /// Block until all queued tracks are processed or `timeout` elapses. Returns true if idle.
    /// Intended for tests and tooling; never call this from the UI thread.
    pub fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.shared.pending.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Signal all threads to stop, abort any running ffmpeg call and join the threads.
    pub fn shutdown(&mut self) {
        {
            let _work = self.shared.work();
            self.shared.stop.store(true, Ordering::SeqCst);
        }
        self.shared.ready.notify_all();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for AnalysisWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(shared: Arc<Shared>, db: AnalysisDatabase) {
    loop {
        let path = {
            let mut work = shared.work();
            loop {
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(path) = work.queue.pop() {
                    work.in_flight += 1;
                    break path;
                }
                // Queued work and the stop flag change under this lock before `ready` is
                // signalled, so no wakeup is missed between the checks above and this wait.
                work = shared.ready.wait(work).unwrap_or_else(|e| e.into_inner());
            }
        };

        // A panic costs this track only: the thread goes on, and the pending count comes down.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process_track(&path, &shared, &db)));
        if outcome.is_err() {
            shared.failed.fetch_add(1, Ordering::SeqCst);
            crate::log_info!("analysis", "Analysis panicked for '{}'; skipped", path.display());
        }
        let mut work = shared.work();
        work.in_flight -= 1;
        shared.publish_pending(&work);
    }
}

fn process_track(path: &Path, shared: &Shared, db: &AnalysisDatabase) {
    let Some((mtime, file_size)) = file_stamp(path) else {
        return;
    };

    if let Some(existing) = db.get_track(path) {
        if row_is_current(&existing, (mtime, file_size)) {
            shared.skipped.fetch_add(1, Ordering::SeqCst);
            return;
        }
    }

    let result = (shared.analyse)(path, &shared.stop);
    // Never persist (or count) a half-finished analysis interrupted by shutdown.
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    let features = match result {
        Ok(features) => features,
        Err(AnalysisError::AcousticFailed) => {
            // Not stored, so the stamp check above does not skip it next session.
            shared.failed.fetch_add(1, Ordering::SeqCst);
            crate::log_info!(
                "analysis",
                "Acoustic analysis failed for '{}'; will retry later",
                path.display()
            );
            return;
        }
        Err(AnalysisError::Unreadable | AnalysisError::Cancelled) => return,
    };
    match db.upsert_track(&features) {
        Ok(()) => {
            shared.analyzed.fetch_add(1, Ordering::SeqCst);
        }
        Err(e) => {
            crate::log_info!("analysis", "Failed to store features for '{}': {}", path.display(), e);
        }
    }
}

/// (mtime seconds, size bytes) used to detect stale rows.
fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let md = std::fs::metadata(path).ok()?;
    md.is_file().then(|| stamp_of(&md))
}

/// The (mtime seconds, size bytes) a row is current for, from a file's metadata: a row whose
/// `mtime` and `file_size` match it is skipped by the worker.
pub fn stamp_of(md: &std::fs::Metadata) -> (u64, u64) {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (mtime, md.len())
}

/// Whether `row` is current for a file stamped `stamp` ([`stamp_of`]), as the worker checks.
pub fn row_is_current(row: &TrackFeatures, stamp: (u64, u64)) -> bool {
    (row.mtime, row.file_size) == stamp
}

/// Why [`analyze_track`] produced no features. None of these may be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisError {
    /// The path is missing or not a regular file.
    Unreadable,
    /// `cancel` was raised before the analysis finished.
    Cancelled,
    /// No BPM tag, and the ffmpeg acoustic pass failed (ffmpeg missing, error exit, timeout or
    /// no audio decoded). Transient: the track should be retried later.
    AcousticFailed,
}

/// Analyse a single track: embedded tags first, ffmpeg acoustic analysis only if no BPM tag.
/// Never modifies the file.
pub fn analyze_track(path: &Path, cancel: Option<&AtomicBool>) -> Result<TrackFeatures, AnalysisError> {
    analyze_track_with(&FfmpegConfig::default(), path, cancel)
}

fn analyze_track_with(
    ffmpeg: &FfmpegConfig,
    path: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<TrackFeatures, AnalysisError> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::SeqCst));
    let (mtime, file_size) = file_stamp(path).ok_or(AnalysisError::Unreadable)?;
    let tags = read_tags(path);

    let (h_title, h_artist, h_album) = crate::metadata::parse_path_heuristics(path);
    let or_else =
        |tagged: Option<String>, fallback: String| tagged.filter(|s| !s.trim().is_empty()).unwrap_or(fallback);

    let mut features = TrackFeatures {
        filepath: path.to_path_buf(),
        mtime,
        file_size,
        title: or_else(tags.title, h_title),
        artist: or_else(tags.artist, h_artist),
        album: or_else(tags.album, h_album),
        genre: tags.genre.unwrap_or_default(),
        year: tags.year,
        bpm: tags.bpm,
        camelot_key: tags
            .initial_key
            .as_deref()
            .and_then(MusicalKey::parse_to_camelot)
            .map(|k| k.to_string()),
        initial_key: tags.initial_key,
        ..Default::default()
    };

    if features.bpm.is_none() {
        if cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        let acoustic = BpmAnalyzer::analyze_acoustic_with(ffmpeg, path, cancel);
        if cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        let acoustic = acoustic.ok_or(AnalysisError::AcousticFailed)?;
        features.bpm = acoustic.bpm;
        features.energy = acoustic.energy;
    }

    if cancelled() {
        return Err(AnalysisError::Cancelled);
    }

    features.analyzed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(features)
}

#[derive(Default)]
struct TagInfo {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    genre: Option<String>,
    year: Option<u32>,
    bpm: Option<f64>,
    initial_key: Option<String>,
}

fn read_tags(path: &Path) -> TagInfo {
    use lofty::config::ParseOptions;
    use lofty::prelude::*;
    use lofty::probe::Probe;
    use lofty::tag::ItemKey;

    let Ok(probe) = Probe::open(path) else {
        return TagInfo::default();
    };
    let Ok(tagged_file) = probe.options(ParseOptions::new().read_cover_art(false)).read() else {
        return TagInfo::default();
    };

    // Prefer the primary tag, then fall back to any other tag present in the file.
    let primary = tagged_file.primary_tag();
    let primary_type = primary.map(|t| t.tag_type());
    let mut tags: Vec<&lofty::tag::Tag> = primary.into_iter().collect();
    tags.extend(tagged_file.tags().iter().filter(|t| Some(t.tag_type()) != primary_type));

    let text = |f: &dyn Fn(&lofty::tag::Tag) -> Option<String>| {
        tags.iter().find_map(|t| f(t).filter(|s| !s.trim().is_empty()))
    };

    TagInfo {
        title: text(&|t| t.title().map(|s| s.to_string())),
        artist: text(&|t| t.artist().map(|s| s.to_string())),
        album: text(&|t| t.album().map(|s| s.to_string())),
        genre: text(&|t| t.genre().map(|s| s.to_string())),
        year: tags.iter().find_map(|t| t.date()).map(|d| d.year as u32),
        bpm: tags.iter().find_map(|t| BpmAnalyzer::bpm_from_tag(t)),
        initial_key: text(&|t| t.get_string(ItemKey::InitialKey).map(|s| s.trim().to_string())),
    }
}
