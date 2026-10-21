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
    /// left out, so it never pushes out its own first ones. A path still waiting from an earlier
    /// run moves to the back and counts as this run's, so it is not pushed out either. Returns
    /// whether any was added.
    pub fn push_candidates<I: IntoIterator<Item = PathBuf>>(&mut self, paths: I) -> bool {
        let mut added = 0;
        let mut any_new = false;
        for path in paths {
            if added == self.candidate_limit {
                break;
            }
            if let Some(at) = self.candidates.iter().position(|c| *c == path) {
                self.candidates.remove(at);
                self.candidates.push_back(path);
                added += 1;
            } else if self.push_candidate(path) {
                added += 1;
                any_new = true;
            }
        }
        any_new
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

/// Whether `row` is current for a file stamped `stamp` ([`stamp_of`]), as the worker checks: the
/// stamp matches and the row is a full analysis (a tag-only index row still waiting for its
/// acoustic pass, `needs_acoustic`, is not).
pub fn row_is_current(row: &TrackFeatures, stamp: (u64, u64)) -> bool {
    row_stamp_matches(row, stamp) && !row.needs_acoustic
}

/// Whether `row` was written for a file stamped `stamp`, whatever it still needs: the library
/// index uses it to tell which files it needs to read again.
pub fn row_stamp_matches(row: &TrackFeatures, stamp: (u64, u64)) -> bool {
    (row.mtime, row.file_size) == stamp
}

/// Why [`tag_features`] could not read a file's tags. A file that parses badly or has no tags is
/// not an error (it gets a row from its path); only a failure to read the file is, so an I/O
/// error on a network share is never stored as an untagged file.
#[derive(Debug)]
pub enum TagReadError {
    /// Opening or reading the file failed.
    Io(std::io::Error),
}

impl std::fmt::Display for TagReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TagReadError::Io(e) => write!(f, "couldn't read the file: {e}"),
        }
    }
}

impl std::error::Error for TagReadError {}

/// A track's row from its tags alone, for a file stamped `stamp` (the tag half of
/// [`analyze_track`]): the same path fallbacks for a missing title, artist or album, the Camelot
/// key from the tagged key, and `needs_acoustic` set when there is no BPM tag. Never modifies the
/// file and never runs ffmpeg.
pub fn tag_features(path: &Path, stamp: (u64, u64)) -> Result<TrackFeatures, TagReadError> {
    let tags = read_tags_checked(path)?;
    let mut features = features_from_tags(path, stamp, tags);
    features.needs_acoustic = features.bpm.is_none();
    features.analyzed_at = now_secs();
    Ok(features)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn features_from_tags(path: &Path, (mtime, file_size): (u64, u64), tags: TagInfo) -> TrackFeatures {
    let (h_title, h_artist, h_album) = crate::metadata::parse_path_heuristics(path);
    let or_else =
        |tagged: Option<String>, fallback: String| tagged.filter(|s| !s.trim().is_empty()).unwrap_or(fallback);

    TrackFeatures {
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
    }
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
    let stamp = file_stamp(path).ok_or(AnalysisError::Unreadable)?;
    // As before #41, tags that can't be read leave the path fallbacks and the acoustic pass.
    let tags = read_tags_checked(path).unwrap_or_default();
    let mut features = features_from_tags(path, stamp, tags);

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

    // A full analysis: stored with the flag cleared whatever the BPM came out as.
    features.needs_acoustic = false;
    features.analyzed_at = now_secs();
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

/// The operating system error at the root of a lofty error, if the file couldn't be opened or
/// read (a network share that failed, a file that went away). lofty reports a file that parses
/// badly as an I/O error too: `InvalidInput` / `UnexpectedEof` without an OS error code, or an
/// OS `EINVAL` when it seeks outside a short file. Those are left out: reading the file again
/// won't help.
fn io_failure(err: &(dyn std::error::Error + 'static)) -> Option<std::io::Error> {
    use std::io::ErrorKind;
    let mut source = Some(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            let parse_problem = matches!(
                io.kind(),
                ErrorKind::InvalidInput | ErrorKind::InvalidData | ErrorKind::UnexpectedEof
            );
            return match io.raw_os_error() {
                Some(code) if !parse_problem => Some(std::io::Error::from_raw_os_error(code)),
                _ => None,
            };
        }
        source = e.source();
    }
    None
}

/// A file's tags. A file that can't be opened or read fails with [`TagReadError::Io`]; one that
/// parses badly, or has no tags, gives empty tags.
fn read_tags_checked(path: &Path) -> Result<TagInfo, TagReadError> {
    use lofty::config::ParseOptions;
    use lofty::prelude::*;
    use lofty::probe::Probe;
    use lofty::tag::ItemKey;

    let fail = |e: lofty::error::FileParseError| match io_failure(&e) {
        Some(io) => Err(TagReadError::Io(io)),
        None => Ok(TagInfo::default()),
    };
    let probe = match Probe::open(path) {
        Ok(probe) => probe,
        Err(e) => return fail(e),
    };
    let tagged_file = match probe.options(ParseOptions::new().read_cover_art(false)).read() {
        Ok(file) => file,
        Err(e) => return fail(e),
    };

    // Prefer the primary tag, then fall back to any other tag present in the file.
    let primary = tagged_file.primary_tag();
    let primary_type = primary.map(|t| t.tag_type());
    let mut tags: Vec<&lofty::tag::Tag> = primary.into_iter().collect();
    tags.extend(tagged_file.tags().iter().filter(|t| Some(t.tag_type()) != primary_type));

    let text = |f: &dyn Fn(&lofty::tag::Tag) -> Option<String>| {
        tags.iter().find_map(|t| f(t).filter(|s| !s.trim().is_empty()))
    };

    Ok(TagInfo {
        title: text(&|t| t.title().map(|s| s.to_string())),
        artist: text(&|t| t.artist().map(|s| s.to_string())),
        album: text(&|t| t.album().map(|s| s.to_string())),
        genre: text(&|t| t.genre().map(|s| s.to_string())),
        year: tags.iter().find_map(|t| t.date()).map(|d| d.year as u32),
        bpm: tags.iter().find_map(|t| BpmAnalyzer::bpm_from_tag(t)),
        initial_key: text(&|t| t.get_string(ItemKey::InitialKey).map(|s| s.trim().to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "quickstream_tag_features_{}_{}_{}",
            name,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn silent_wav(path: &Path) {
        let data_len = 400u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&8000u32.to_le_bytes());
        b.extend_from_slice(&16000u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        b.extend(std::iter::repeat_n(0u8, data_len as usize));
        std::fs::write(path, b).unwrap();
    }

    /// An untagged file gets a row from its path, waiting for its acoustic pass.
    #[test]
    fn an_untagged_file_gets_a_heuristics_row_that_needs_acoustics() {
        let dir = scratch("untagged");
        let track = dir.join("Some Artist - Some Song.wav");
        silent_wav(&track);
        let row = tag_features(&track, (11, 22)).unwrap();
        let (title, artist, album) = crate::metadata::parse_path_heuristics(&track);
        assert_eq!((row.title, row.artist, row.album), (title, artist, album));
        assert_eq!((row.mtime, row.file_size), (11, 22));
        assert_eq!((row.bpm, row.energy), (None, None));
        assert!(row.needs_acoustic);
        assert!(row.analyzed_at > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A BPM tag is all the acoustic pass would add for matching, so the row needs none.
    #[test]
    fn a_bpm_tagged_file_needs_no_acoustic_pass() {
        use lofty::config::WriteOptions;
        use lofty::prelude::*;
        use lofty::tag::{ItemKey, Tag, TagType};

        let dir = scratch("tagged");
        let track = dir.join("tagged.wav");
        silent_wav(&track);
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Tagged".to_string());
        tag.insert_text(ItemKey::IntegerBpm, "124".to_string());
        tag.insert_text(ItemKey::InitialKey, "Am".to_string());
        tag.save_to_path(&track, WriteOptions::default()).unwrap();

        let row = tag_features(&track, (1, 2)).unwrap();
        assert_eq!(row.title, "Tagged");
        assert_eq!(row.bpm, Some(124.0));
        assert_eq!(row.camelot_key.as_deref(), Some("8A"));
        assert!(!row.needs_acoustic);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that parses badly is not an I/O error: it gets a row from its path.
    #[test]
    fn a_corrupt_file_gets_a_heuristics_row() {
        let dir = scratch("corrupt");
        let track = dir.join("broken.mp3");
        std::fs::write(&track, b"not really an mp3 at all").unwrap();
        let row = tag_features(&track, (1, 2)).unwrap();
        assert!(row.needs_acoustic);
        let truncated = dir.join("short.wav");
        std::fs::write(&truncated, b"RIFF\x24\x00\x00\x00WAVEfmt ").unwrap();
        assert!(tag_features(&truncated, (1, 2)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that can't be read is an error, so no row is stored for it.
    #[test]
    fn an_unreadable_file_is_an_io_error() {
        let dir = scratch("missing");
        let gone = dir.join("gone.flac");
        assert!(matches!(tag_features(&gone, (1, 2)), Err(TagReadError::Io(_))));
        let folder = dir.join("folder.flac");
        std::fs::create_dir_all(&folder).unwrap();
        assert!(
            tag_features(&folder, (1, 2)).is_err(),
            "a directory can't be read as a file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn row_is_current_needs_the_stamp_and_a_full_analysis() {
        let mut row = TrackFeatures {
            mtime: 3,
            file_size: 4,
            ..Default::default()
        };
        assert!(row_is_current(&row, (3, 4)));
        assert!(!row_is_current(&row, (3, 5)));
        row.needs_acoustic = true;
        assert!(!row_is_current(&row, (3, 4)));
        assert!(row_stamp_matches(&row, (3, 4)));
        assert!(!row_stamp_matches(&row, (2, 4)));
    }
}
