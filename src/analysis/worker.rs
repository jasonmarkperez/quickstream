//! Background track analysis worker.
//!
//! Fills `track_features` for tracks discovered by the library scanner / autopilot so that
//! `DjMixer::sequence_mix` and the BPM / mood profiles have real BPM, key and energy data.
//!
//! - Embedded tags first (BPM, initial key) via lofty; ffmpeg acoustic analysis only when the
//!   BPM tag is missing.
//! - Resumable: tracks whose row already matches the file's mtime and size are skipped.
//! - Bounded concurrency (a small fixed pool of threads), never on the UI thread.
//! - Read-only towards the music library: the only writes go to the analysis database.
//! - Stops promptly: the stop flag is checked between tracks and while ffmpeg runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::bpm::BpmAnalyzer;
use super::db::{AnalysisDatabase, TrackFeatures};
use super::harmonic::MusicalKey;

/// Snapshot of worker progress counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalysisProgress {
    /// Tracks analysed and written to the database this session.
    pub analyzed: usize,
    /// Tracks skipped because the database already had an up-to-date row.
    pub skipped: usize,
    /// Tracks queued but not yet processed.
    pub pending: usize,
}

struct Shared {
    stop: AtomicBool,
    analyzed: AtomicUsize,
    skipped: AtomicUsize,
    pending: AtomicUsize,
    seen: Mutex<HashSet<PathBuf>>,
}

/// Handle to the background analysis thread pool. Dropping it stops and joins the threads.
pub struct AnalysisWorker {
    tx: Option<Sender<PathBuf>>,
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl AnalysisWorker {
    /// Spawn `threads` (clamped to 1..=2) analysis threads writing into `db`.
    pub fn spawn(db: AnalysisDatabase, threads: usize) -> Self {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let rx = Arc::new(Mutex::new(rx));
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            analyzed: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            seen: Mutex::new(HashSet::new()),
        });

        let handles = (0..threads.clamp(1, 2))
            .filter_map(|i| {
                let rx = Arc::clone(&rx);
                let shared = Arc::clone(&shared);
                let db = db.clone();
                std::thread::Builder::new()
                    .name(format!("qs-analysis-{}", i))
                    .spawn(move || worker_loop(rx, shared, db))
                    .ok()
            })
            .collect();

        Self {
            tx: Some(tx),
            shared,
            handles,
        }
    }

    /// Queue tracks for analysis. Paths already queued this session are ignored.
    pub fn enqueue<I: IntoIterator<Item = PathBuf>>(&self, paths: I) {
        let Some(tx) = self.tx.as_ref() else { return };
        let mut seen = self.shared.seen.lock().unwrap();
        for path in paths {
            if seen.insert(path.clone()) {
                self.shared.pending.fetch_add(1, Ordering::SeqCst);
                if tx.send(path).is_err() {
                    self.shared.pending.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }

    /// Current progress counters (lock-free).
    pub fn progress(&self) -> AnalysisProgress {
        AnalysisProgress {
            analyzed: self.shared.analyzed.load(Ordering::SeqCst),
            skipped: self.shared.skipped.load(Ordering::SeqCst),
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
        self.shared.stop.store(true, Ordering::SeqCst);
        self.tx.take();
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

fn worker_loop(rx: Arc<Mutex<Receiver<PathBuf>>>, shared: Arc<Shared>, db: AnalysisDatabase) {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let next = {
            let rx = rx.lock().unwrap();
            rx.recv_timeout(Duration::from_millis(100))
        };
        let path = match next {
            Ok(p) => p,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        process_track(&path, &shared, &db);
        shared.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

fn process_track(path: &Path, shared: &Shared, db: &AnalysisDatabase) {
    let Some((mtime, file_size)) = file_stamp(path) else { return };

    if let Some(existing) = db.get_track(path) {
        if existing.mtime == mtime && existing.file_size == file_size {
            shared.skipped.fetch_add(1, Ordering::SeqCst);
            return;
        }
    }

    let Some(features) = analyze_track(path, Some(&shared.stop)) else { return };
    // Never persist a half-finished analysis interrupted by shutdown.
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
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
    if !md.is_file() {
        return None;
    }
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((mtime, md.len()))
}

/// Analyse a single track: embedded tags first, ffmpeg acoustic analysis only if no BPM tag.
/// Returns `None` if the file cannot be read or `cancel` is raised. Never modifies the file.
pub fn analyze_track(path: &Path, cancel: Option<&AtomicBool>) -> Option<TrackFeatures> {
    let (mtime, file_size) = file_stamp(path)?;
    let tags = read_tags(path);

    let (h_title, h_artist, h_album) = crate::metadata::parse_path_heuristics(path);
    let or_else = |tagged: Option<String>, fallback: String| tagged.filter(|s| !s.trim().is_empty()).unwrap_or(fallback);

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
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            return None;
        }
        let acoustic = BpmAnalyzer::analyze_acoustic(path, cancel);
        features.bpm = acoustic.bpm;
        features.energy = acoustic.energy;
    }

    if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
        return None;
    }

    features.analyzed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(features)
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

    let Ok(probe) = Probe::open(path) else { return TagInfo::default() };
    let Ok(tagged_file) = probe.options(ParseOptions::new().read_cover_art(false)).read() else {
        return TagInfo::default();
    };

    // Prefer the primary tag, then fall back to any other tag present in the file.
    let primary = tagged_file.primary_tag();
    let primary_type = primary.map(|t| t.tag_type());
    let mut tags: Vec<&lofty::tag::Tag> = primary.into_iter().collect();
    tags.extend(tagged_file.tags().iter().filter(|t| Some(t.tag_type()) != primary_type));

    let text = |f: &dyn Fn(&lofty::tag::Tag) -> Option<String>| tags.iter().find_map(|t| f(t).filter(|s| !s.trim().is_empty()));

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
