//! Library directory indexing (walkdir), JSONL caching, and Skim fuzzy search integration.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use skim::prelude::*;
use walkdir::WalkDir;

use crate::metadata::{extract_metadata, is_supported_audio_file, AudioMetadata};
use crate::paths;

#[derive(Debug, Clone, PartialEq)]
pub enum AutopilotEvent {
    Track(AudioMetadata),
    Finished(usize),
}

/// In-memory metadata index keyed by file path, shared between the UI and background scanners.
/// Clones share one map, so handing it to a worker costs a reference count instead of a copy of
/// the whole library (issue #4). Lookups take a read lock only for the duration of the call.
#[derive(Debug, Clone, Default)]
pub struct MetadataCache(Arc<std::sync::RwLock<HashMap<PathBuf, AudioMetadata>>>);

impl MetadataCache {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<PathBuf, AudioMetadata>> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, path: &Path) -> Option<AudioMetadata> {
        self.read().get(path).cloned()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.read().contains_key(path)
    }

    /// Insert or replace the entry for `meta.filepath`.
    pub fn insert(&self, meta: AudioMetadata) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(meta.filepath.clone(), meta);
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Whether both handles refer to the same underlying map.
    pub fn shares_storage_with(&self, other: &MetadataCache) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl From<HashMap<PathBuf, AudioMetadata>> for MetadataCache {
    fn from(map: HashMap<PathBuf, AudioMetadata>) -> Self {
        Self(Arc::new(std::sync::RwLock::new(map)))
    }
}

/// A track item formatted for Skim fuzzy searching with a rich metadata preview window.
pub struct SkimTrackItem {
    pub metadata: AudioMetadata,
}

impl SkimItem for SkimTrackItem {
    fn text(&self) -> Cow<'_, str> {
        let artist_part = if !self.metadata.artist.is_empty() {
            format!("{} - ", self.metadata.artist)
        } else {
            String::new()
        };
        Cow::Owned(format!(
            "{}{} [{}]",
            artist_part, self.metadata.title, self.metadata.format_name
        ))
    }

    fn output(&self) -> Cow<'_, str> {
        Cow::Owned(self.metadata.filepath.to_string_lossy().to_string())
    }

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
        let text = format!(
            "Title:    {}\nArtist:   {}\nAlbum:    {}\nGenre:    {}\nDuration: {}\nFormat:   {} ({} kbps, {} Hz)\nFile:     {}",
            self.metadata.title,
            if self.metadata.artist.is_empty() { "<Unknown>" } else { &self.metadata.artist },
            if self.metadata.album.is_empty() { "<Unknown>" } else { &self.metadata.album },
            if self.metadata.genre.is_empty() { "<Unknown>" } else { &self.metadata.genre },
            self.metadata.formatted_duration(),
            self.metadata.format_name,
            self.metadata.bitrate_kbps,
            self.metadata.sample_rate_hz,
            self.metadata.filepath.display()
        );
        ItemPreview::Text(text)
    }
}

/// Remembers paths visited during a directory walk. Some SMB shares (seen on macOS with a
/// Synology NAS) return each directory entry several times; without this every duplicate
/// subtree is walked again and its tracks are collected once per duplicate (issue #35).
#[derive(Default)]
pub struct SeenPaths(std::collections::HashSet<PathBuf>);

impl SeenPaths {
    /// True the first time `path` is seen.
    pub fn first_visit(&mut self, path: &Path) -> bool {
        self.0.insert(path.to_path_buf())
    }
}

/// Bounds for gathering profile-mix candidates on large libraries (issue #38).
#[derive(Debug, Clone, Copy)]
pub struct ProfileScanLimits {
    /// Stop after this many top-level folders even if few tracks matched.
    pub max_folders: usize,
    /// Stop once gathering has taken this long (at least one folder is always scanned).
    pub time_budget: Duration,
}

impl Default for ProfileScanLimits {
    fn default() -> Self {
        Self {
            max_folders: 500,
            time_budget: Duration::from_secs(20),
        }
    }
}

/// Counters and timings for one autopilot mix build, written to the log as `[autopilot]` PERF
/// lines so slow mixes show where the time goes (folder walks vs tag reads vs matching).
#[derive(Default)]
struct MixScanStats {
    folders: AtomicUsize,
    files: AtomicUsize,
    /// Summed across worker threads, so it can exceed wall-clock time.
    walk: AtomicU64,
    metadata: AtomicU64,
    cache_hits: AtomicUsize,
    db_hits: AtomicUsize,
    tag_reads: AtomicUsize,
    tag_read_time: AtomicU64,
    fallbacks: AtomicUsize,
    matching: AtomicU64,
}

impl MixScanStats {
    fn count(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn time(nanos: &AtomicU64, since: Instant) {
        nanos.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn summary(&self) -> String {
        let d = |nanos: &AtomicU64| Duration::from_nanos(nanos.load(Ordering::Relaxed));
        format!(
            "folders={} files={} walk={:.2?} metadata={:.2?} (cache_hits={} db_hits={} tag_reads={} in {:.2?} fallbacks={}) matching={:.2?}",
            self.folders.load(Ordering::Relaxed),
            self.files.load(Ordering::Relaxed),
            d(&self.walk),
            d(&self.metadata),
            self.cache_hits.load(Ordering::Relaxed),
            self.db_hits.load(Ordering::Relaxed),
            self.tag_reads.load(Ordering::Relaxed),
            d(&self.tag_read_time),
            self.fallbacks.load(Ordering::Relaxed),
            d(&self.matching),
        )
    }
}

/// Worker threads that list folders and read tags while gathering profile-mix candidates.
const MIX_SCAN_THREADS: usize = 4;
/// Tracks per folder whose tags are read while gathering profile-mix candidates.
const TAG_READS_PER_FOLDER: usize = 4;

fn log_autopilot(message: String) {
    crate::logger::log_raw("PERF", "autopilot", &message);
}

#[derive(Default)]
pub struct LibraryManager {
    pub tracks: Vec<AudioMetadata>,
}

impl LibraryManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load library from cached music_index.jsonl if present.
    pub fn load_cache() -> Vec<AudioMetadata> {
        let path = paths::get_music_index_path();
        if !path.exists() {
            return Vec::new();
        }

        let mut tracks = Vec::new();
        if let Ok(file) = File::open(&path) {
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(track) = serde_json::from_str::<AudioMetadata>(&line) {
                    tracks.push(track);
                }
            }
        }
        tracks
    }

    /// Load library from cached music_index.jsonl into a HashMap for fast key lookup.
    pub fn load_cache_map() -> HashMap<PathBuf, AudioMetadata> {
        let path = paths::get_music_index_path();
        if !path.exists() {
            return HashMap::new();
        }

        let mut map = HashMap::new();
        if let Ok(file) = File::open(&path) {
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(track) = serde_json::from_str::<AudioMetadata>(&line) {
                    map.insert(track.filepath.clone(), track);
                }
            }
        }
        map
    }

    /// Load cached tracks that reside within the given directory.
    pub fn load_cache_for_dir(dir: &Path) -> Vec<AudioMetadata> {
        let cache = Self::load_cache();
        cache.into_iter().filter(|t| t.filepath.starts_with(dir)).collect()
    }

    /// Save library tracks to cached music_index.jsonl.
    pub fn save_cache(tracks: &[AudioMetadata]) -> bool {
        let path = paths::get_music_index_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(mut file) = File::create(&path) {
            for track in tracks {
                if let Ok(json) = serde_json::to_string(track) {
                    let _ = writeln!(file, "{}", json);
                }
            }
            return true;
        }
        false
    }

    /// Append a single track's metadata to cached music_index.jsonl.
    pub fn append_to_cache(track: &AudioMetadata) {
        let path = paths::get_music_index_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            if let Ok(json) = serde_json::to_string(track) {
                let _ = writeln!(file, "{}", json);
            }
        }
    }

    /// Scan a directory recursively using walkdir and extract metadata.
    pub fn scan_directory(dir: &Path) -> Vec<AudioMetadata> {
        Self::scan_directory_scoped(dir)
    }

    /// Scan a directory and all subdirectories, avoiding symlink loops and hidden directories,
    /// reusing metadata from cache when file size matches to avoid disk I/O.
    pub fn scan_directory_scoped(dir: &Path) -> Vec<AudioMetadata> {
        let cache = Self::load_cache_map();
        Self::scan_directory_scoped_with_cache(dir, &cache)
    }

    /// Derives intelligent heuristic title, artist, and album from file and folder paths
    /// without reading tags from disk, ensuring instant zero-cost initial display.
    pub fn parse_path_heuristics(path: &Path) -> (String, String, String) {
        crate::metadata::parse_path_heuristics(path)
    }

    pub fn scan_directory_scoped_with_cache(
        dir: &Path,
        cache: &HashMap<PathBuf, AudioMetadata>,
    ) -> Vec<AudioMetadata> {
        let mut tracks = Vec::new();

        // 1. Inspect immediate entries in dir: check if dir has subdirectories
        let mut subdirs = Vec::new();
        let mut immediate_files = Vec::new();

        let mut seen = SeenPaths::default();

        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if !seen.first_visit(&entry.path()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if (name.starts_with('.') && name != "." && name != "..")
                    || name == "#recycle"
                    || name == "$RECYCLE.BIN"
                {
                    continue;
                }
                let path = entry.path();
                let is_dir = match entry.file_type() {
                    Ok(ft) => {
                        if ft.is_dir() {
                            true
                        } else if ft.is_symlink() {
                            path.is_dir()
                        } else {
                            false
                        }
                    }
                    Err(_) => path.is_dir(),
                };
                if is_dir {
                    subdirs.push(path);
                } else if is_supported_audio_file(&path) {
                    immediate_files.push(path);
                }
            }
        }

        // Helper to process an audio file path quickly without blocking network I/O
        let process_file = |path: &Path, tracks: &mut Vec<AudioMetadata>| {
            let file_len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if let Some(cached) = cache.get(path) {
                if cached.file_size == file_len && !cached.title.is_empty() {
                    tracks.push(cached.clone());
                    return;
                }
            }
            let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("track");
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase();
            let (title, artist, album) = Self::parse_path_heuristics(path);
            tracks.push(AudioMetadata {
                filepath: path.to_path_buf(),
                filename: filename.to_string(),
                title,
                artist,
                album,
                format_name: ext,
                file_size: file_len,
                sample_rate_hz: 44100,
                channels: 2,
                ..Default::default()
            });
        };

        let target_limit = 60;
        let mut rng = crate::queue::FastRng::new();

        // If there are immediate audio files, sample a few if mixed with subdirs, or all if no subdirs
        if subdirs.is_empty() {
            rng.shuffle(&mut immediate_files);
            for f in immediate_files.iter().take(target_limit) {
                process_file(f, &mut tracks);
            }
            return tracks;
        }

        if !immediate_files.is_empty() {
            rng.shuffle(&mut immediate_files);
            for f in immediate_files.iter().take(2) {
                process_file(f, &mut tracks);
            }
        }

        // Diverse sampling across subdirectories:
        rng.shuffle(&mut subdirs);

        if subdirs.len() >= target_limit {
            // Large collection (e.g. 5,000 artist folders):
            // Sample 1 random track per subdirectory to guarantee maximum artist variety
            // and prevent any single artist (e.g. Einaudi) from dominating the queue.
            for subdir in &subdirs {
                if tracks.len() >= target_limit {
                    break;
                }
                let mut folder_tracks = Vec::new();
                let mut seen = SeenPaths::default();
                for entry in WalkDir::new(subdir)
                    .max_depth(3)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        let name = e.file_name().to_string_lossy();
                        if (name.starts_with('.') && name != "." && name != "..")
                            || name == "#recycle"
                            || name == "$RECYCLE.BIN"
                        {
                            return false;
                        }
                        seen.first_visit(e.path())
                    })
                    .filter_map(|e| e.ok())
                {
                    let path = entry.path();
                    if path.is_file() && is_supported_audio_file(path) {
                        folder_tracks.push(path.to_path_buf());
                        if folder_tracks.len() >= 10 {
                            break;
                        }
                    }
                }

                if !folder_tracks.is_empty() {
                    let pick = (rng.next_u64() as usize) % folder_tracks.len();
                    process_file(&folder_tracks[pick], &mut tracks);
                }
            }
        } else {
            // Smaller collection (e.g. 5-30 album folders by an artist):
            // Distribute tracks evenly across the subdirectories.
            let per_subdir = target_limit.div_ceil(subdirs.len()).max(1);
            let mut extra_tracks = Vec::new();

            for subdir in &subdirs {
                let mut folder_tracks = Vec::new();
                let mut seen = SeenPaths::default();
                for entry in WalkDir::new(subdir)
                    .max_depth(3)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        let name = e.file_name().to_string_lossy();
                        if (name.starts_with('.') && name != "." && name != "..")
                            || name == "#recycle"
                            || name == "$RECYCLE.BIN"
                        {
                            return false;
                        }
                        seen.first_visit(e.path())
                    })
                    .filter_map(|e| e.ok())
                {
                    let path = entry.path();
                    if path.is_file() && is_supported_audio_file(path) {
                        folder_tracks.push(path.to_path_buf());
                    }
                }

                if !folder_tracks.is_empty() {
                    rng.shuffle(&mut folder_tracks);
                    for (i, p) in folder_tracks.into_iter().enumerate() {
                        if i < per_subdir && tracks.len() < target_limit {
                            process_file(&p, &mut tracks);
                        } else {
                            extra_tracks.push(p);
                        }
                    }
                }
            }

            // Fill any remaining slots up to target_limit
            if tracks.len() < target_limit && !extra_tracks.is_empty() {
                rng.shuffle(&mut extra_tracks);
                for p in extra_tracks {
                    if tracks.len() >= target_limit {
                        break;
                    }
                    process_file(&p, &mut tracks);
                }
            }
        }

        tracks
    }

    /// Progressive background scanner for Autopilot mode (defaults to DiverseShuffle).
    pub fn scan_directory_autopilot_progressive(
        dir: PathBuf,
        known_subdirs: Vec<PathBuf>,
        cache: HashMap<PathBuf, AudioMetadata>,
        tx: Sender<AutopilotEvent>,
        stop_flag: Arc<AtomicBool>,
    ) {
        let db = crate::analysis::AnalysisDatabase::open_default()
            .unwrap_or_else(|_| crate::analysis::AnalysisDatabase::open_in_memory().unwrap());
        Self::scan_directory_autopilot_with_profile(
            dir,
            known_subdirs,
            cache.into(),
            crate::analysis::MixProfile::DiverseShuffle,
            db,
            tx,
            stop_flag,
        );
    }

    /// Progressive background scanner for Autopilot with mix profiles (BPM Flow, Mood, Seed Radio, Diverse).
    pub fn scan_directory_autopilot_with_profile(
        dir: PathBuf,
        known_subdirs: Vec<PathBuf>,
        cache: MetadataCache,
        profile: crate::analysis::MixProfile,
        db: crate::analysis::AnalysisDatabase,
        tx: Sender<AutopilotEvent>,
        stop_flag: Arc<AtomicBool>,
    ) {
        Self::scan_directory_autopilot_with_profile_limits(
            dir,
            known_subdirs,
            cache,
            profile,
            db,
            tx,
            stop_flag,
            ProfileScanLimits::default(),
        )
    }

    /// Like `scan_directory_autopilot_with_profile`, with explicit bounds on candidate gathering.
    #[allow(clippy::too_many_arguments)]
    pub fn scan_directory_autopilot_with_profile_limits(
        dir: PathBuf,
        known_subdirs: Vec<PathBuf>,
        cache: MetadataCache,
        profile: crate::analysis::MixProfile,
        db: crate::analysis::AnalysisDatabase,
        tx: Sender<AutopilotEvent>,
        stop_flag: Arc<AtomicBool>,
        limits: ProfileScanLimits,
    ) {
        let scan_start = std::time::Instant::now();
        let target_limit = 60;
        let mut total_sent = 0;
        let mut rng = crate::queue::FastRng::new();

        let stats = MixScanStats::default();
        let make_metadata_untimed = |path: &Path| -> AudioMetadata {
            let file_len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if let Some(cached) = cache.get(path) {
                if cached.file_size == file_len && !cached.title.is_empty() {
                    MixScanStats::count(&stats.cache_hits);
                    let mut meta = cached;
                    if meta.genre.is_empty() {
                        if let Some(feat) = db.get_track(path) {
                            if !feat.genre.is_empty() {
                                meta.genre = feat.genre;
                            }
                        }
                    }
                    return meta;
                }
            }
            if let Some(feat) = db.get_track(path) {
                MixScanStats::count(&stats.db_hits);
                let (title, artist, album) = Self::parse_path_heuristics(path);
                return AudioMetadata {
                    filepath: path.to_path_buf(),
                    filename: path.file_name().and_then(|f| f.to_str()).unwrap_or("track").to_string(),
                    title: if !feat.title.is_empty() { feat.title } else { title },
                    artist: if !feat.artist.is_empty() { feat.artist } else { artist },
                    album: if !feat.album.is_empty() { feat.album } else { album },
                    genre: feat.genre,
                    format_name: path.extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase(),
                    file_size: file_len,
                    sample_rate_hz: 44100,
                    channels: 2,
                    ..Default::default()
                };
            }
            MixScanStats::count(&stats.tag_reads);
            let read_start = Instant::now();
            let extracted = crate::metadata::extract_metadata(path);
            MixScanStats::time(&stats.tag_read_time, read_start);
            if let Ok(meta) = extracted {
                return meta;
            }
            MixScanStats::count(&stats.fallbacks);
            let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("track");
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase();
            let (title, artist, album) = Self::parse_path_heuristics(path);
            AudioMetadata {
                filepath: path.to_path_buf(),
                filename: filename.to_string(),
                title,
                artist,
                album,
                format_name: ext,
                file_size: file_len,
                sample_rate_hz: 44100,
                channels: 2,
                ..Default::default()
            }
        };
        let make_metadata = |path: &Path| -> AudioMetadata {
            let start = Instant::now();
            let meta = make_metadata_untimed(path);
            MixScanStats::time(&stats.metadata, start);
            meta
        };

        // 1. Determine subdirectories and immediate files in dir
        let listing_start = Instant::now();
        let (mut subdirs, mut immediate_files) = if known_subdirs.len() >= target_limit {
            (known_subdirs, Vec::new())
        } else {
            let mut found_subdirs = Vec::new();
            let mut found_files = Vec::new();

            let mut seen = SeenPaths::default();

            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if !seen.first_visit(&entry.path()) {
                        continue;
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    if (name.starts_with('.') && name != "." && name != "..")
                        || name == "#recycle"
                        || name == "$RECYCLE.BIN"
                    {
                        continue;
                    }
                    let path = entry.path();
                    let is_dir = match entry.file_type() {
                        Ok(ft) => {
                            if ft.is_dir() {
                                true
                            } else if ft.is_symlink() {
                                path.is_dir()
                            } else {
                                false
                            }
                        }
                        Err(_) => path.is_dir(),
                    };
                    if is_dir {
                        found_subdirs.push(path);
                    } else if is_supported_audio_file(&path) {
                        found_files.push(path);
                    }
                }
            }
            (found_subdirs, found_files)
        };

        log_autopilot(format!(
            "mix start profile=\"{}\" dir='{}' subdirs={} immediate_files={} listing={:.2?}",
            profile.name(),
            dir.display(),
            subdirs.len(),
            immediate_files.len(),
            listing_start.elapsed()
        ));

        // Shuffle subdirectories for fair and diverse library sampling
        rng.shuffle(&mut subdirs);

        // 2. If specialized mix profile is active (BPM Flow, Mood Vibe, or Seed Radio),
        // gather matching candidates across shuffled subdirectories until we have enough matching tracks!
        if profile != crate::analysis::MixProfile::DiverseShuffle {
            let mut matched_candidates: Vec<AudioMetadata> = Vec::new();
            let mut all_candidates: Vec<AudioMetadata> = Vec::new();

            let is_match_untimed = |meta: &AudioMetadata| -> bool {
                match &profile {
                    crate::analysis::MixProfile::BpmFlow { target_bpm, tolerance } => {
                        let min_bpm = target_bpm * (1.0 - tolerance);
                        let max_bpm = target_bpm * (1.0 + tolerance);
                        let feat = db.get_track(&meta.filepath);
                        if let Some(b) = feat.and_then(|f| f.bpm) {
                            (b >= min_bpm && b <= max_bpm)
                                || (b >= min_bpm / 2.0 && b <= max_bpm / 2.0)
                                || (b >= min_bpm * 2.0 && b <= max_bpm * 2.0)
                        } else {
                            false
                        }
                    }
                    crate::analysis::MixProfile::MoodVibe(mood) => {
                        let feat = db.get_track(&meta.filepath);
                        mood.matches_track(meta, feat.as_ref())
                    }
                    crate::analysis::MixProfile::SeedRadio(seed_path) => {
                        if &meta.filepath == seed_path {
                            return false;
                        }
                        let seed_feat = db.get_track(seed_path).unwrap_or_default();
                        let (_seed_title, seed_artist, _seed_album) = Self::parse_path_heuristics(seed_path);
                        let seed_artist = if !seed_feat.artist.is_empty() { seed_feat.artist } else { seed_artist };
                        let seed_genre = seed_feat.genre;

                        let feat = db.get_track(&meta.filepath);
                        let artist_match = !seed_artist.is_empty() && meta.artist.eq_ignore_ascii_case(&seed_artist);
                        let genre_match = !seed_genre.is_empty() && (
                            meta.genre.eq_ignore_ascii_case(&seed_genre)
                            || feat.as_ref().map(|f| f.genre.eq_ignore_ascii_case(&seed_genre)).unwrap_or(false)
                        );
                        artist_match || genre_match
                    }
                    _ => true,
                }
            };
            let is_match = |meta: &AudioMetadata| -> bool {
                let start = Instant::now();
                let matched = is_match_untimed(meta);
                MixScanStats::time(&stats.matching, start);
                matched
            };
            let mut last_progress = Instant::now();
            // The first match is queued at once so playback starts while gathering continues;
            // the rest of the mix is sequenced to follow it.
            let mut first_sent: Option<AudioMetadata> = None;
            let over_budget = || scan_start.elapsed() >= limits.time_budget;

            if !subdirs.is_empty() {
                let per_subdir = if subdirs.len() >= 60 {
                    2
                } else if subdirs.len() >= 20 {
                    3
                } else if subdirs.len() >= 5 {
                    4
                } else {
                    15
                };

                // Include immediate files if any
                if !immediate_files.is_empty() {
                    rng.shuffle(&mut immediate_files);
                    for f in immediate_files.iter().take(per_subdir) {
                        let meta = make_metadata(f);
                        if is_match(&meta) {
                            if first_sent.is_none() && tx.send(AutopilotEvent::Track(meta.clone())).is_ok() {
                                total_sent += 1;
                                first_sent = Some(meta);
                                continue;
                            }
                            matched_candidates.push(meta.clone());
                        }
                        all_candidates.push(meta);
                    }
                }

                // Walk shuffled subdirectories until we have 80 matching tracks, or have visited
                // `max_folders` subdirectories, or the time budget is spent (after at least one
                // folder). Shortfalls are filled from diverse candidates below.
                //
                // Tag reads over a network share are latency-bound, so folders are listed and read
                // by a few worker threads, and only a few tracks per folder are read: within the
                // time budget the mix then draws from many more artists.
                let max_folders = limits.max_folders.min(subdirs.len());
                let next_folder = AtomicUsize::new(0);
                let halt = AtomicBool::new(false);
                std::thread::scope(|scope| {
                    let (folder_tx, folder_rx) = std::sync::mpsc::channel::<(usize, Vec<AudioMetadata>)>();
                    for worker in 0..MIX_SCAN_THREADS {
                        let folder_tx = folder_tx.clone();
                        let (next_folder, halt, subdirs, stop_flag, stats) =
                            (&next_folder, &halt, &subdirs, &stop_flag, &stats);
                        let make_metadata = &make_metadata;
                        scope.spawn(move || {
                            let mut rng = crate::queue::FastRng::new();
                            for _ in 0..worker {
                                rng.next_u64();
                            }
                            loop {
                                if halt.load(Ordering::Relaxed) || stop_flag.load(Ordering::Relaxed) {
                                    break;
                                }
                                let idx = next_folder.fetch_add(1, Ordering::Relaxed);
                                if idx >= max_folders {
                                    break;
                                }
                                let walk_start = Instant::now();
                                let mut folder_tracks = Vec::new();
                                let mut seen = SeenPaths::default();
                                for entry in WalkDir::new(&subdirs[idx])
                                    .max_depth(3)
                                    .follow_links(false)
                                    .into_iter()
                                    .filter_entry(|e| {
                                        let name = e.file_name().to_string_lossy();
                                        !name.starts_with('.') && name != "#recycle" && name != "$RECYCLE.BIN" && seen.first_visit(e.path())
                                    })
                                    .filter_map(|e| e.ok())
                                {
                                    if halt.load(Ordering::Relaxed) || stop_flag.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    let path = entry.path();
                                    if path.is_file() && is_supported_audio_file(path) {
                                        folder_tracks.push(path.to_path_buf());
                                        if folder_tracks.len() >= 30 {
                                            break;
                                        }
                                    }
                                }
                                MixScanStats::time(&stats.walk, walk_start);

                                let files_found = folder_tracks.len();
                                rng.shuffle(&mut folder_tracks);
                                let mut metas = Vec::new();
                                for p in folder_tracks.iter().take(TAG_READS_PER_FOLDER) {
                                    if halt.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    metas.push(make_metadata(p));
                                }
                                if folder_tx.send((files_found, metas)).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    drop(folder_tx);

                    let mut folders_done = 0;
                    for (files_found, metas) in folder_rx {
                        folders_done += 1;
                        stats.folders.fetch_add(1, Ordering::Relaxed);
                        stats.files.fetch_add(files_found, Ordering::Relaxed);

                        let mut artist_match_count = 0;
                        for meta in metas {
                            if is_match(&meta) {
                                if first_sent.is_none() && tx.send(AutopilotEvent::Track(meta.clone())).is_ok() {
                                    total_sent += 1;
                                    first_sent = Some(meta);
                                    artist_match_count += 1;
                                    continue;
                                }
                                if artist_match_count < per_subdir && matched_candidates.len() < 120 {
                                    matched_candidates.push(meta.clone());
                                    all_candidates.push(meta);
                                    artist_match_count += 1;
                                }
                            } else if all_candidates.len() < 200 {
                                all_candidates.push(meta);
                            }
                        }

                        if folders_done % 50 == 0 || last_progress.elapsed() >= Duration::from_secs(10) {
                            last_progress = Instant::now();
                            log_autopilot(format!(
                                "mix progress folders={}/{} files={} matched={} candidates={} elapsed={:.2?}",
                                folders_done,
                                subdirs.len(),
                                stats.files.load(Ordering::Relaxed),
                                matched_candidates.len(),
                                all_candidates.len(),
                                scan_start.elapsed()
                            ));
                        }

                        if stop_flag.load(Ordering::Relaxed) || matched_candidates.len() >= 80 || over_budget() {
                            break;
                        }
                    }
                    halt.store(true, Ordering::Relaxed);
                });
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
            } else {
                // Flat folder with no subdirectories: sample directly from files
                let mut all_files = immediate_files.clone();
                if all_files.is_empty() {
                    let mut seen = SeenPaths::default();
                    for entry in WalkDir::new(&dir)
                        .max_depth(3)
                        .follow_links(false)
                        .into_iter()
                        .filter_entry(|e| {
                            let name = e.file_name().to_string_lossy();
                            !name.starts_with('.') && name != "#recycle" && name != "$RECYCLE.BIN" && seen.first_visit(e.path())
                        })
                        .filter_map(|e| e.ok())
                    {
                        if stop_flag.load(Ordering::Relaxed) {
                            return;
                        }
                        let p = entry.path();
                        if p.is_file() && is_supported_audio_file(p) {
                            all_files.push(p.to_path_buf());
                            if all_files.len() >= 200 {
                                break;
                            }
                        }
                    }
                }
                stats.files.fetch_add(all_files.len().min(200), Ordering::Relaxed);
                rng.shuffle(&mut all_files);
                for (i, p) in all_files.into_iter().take(200).enumerate() {
                    if i > 0 && over_budget() {
                        break;
                    }
                    let meta = make_metadata(&p);
                    if is_match(&meta) {
                        if first_sent.is_none() && tx.send(AutopilotEvent::Track(meta.clone())).is_ok() {
                            total_sent += 1;
                            first_sent = Some(meta);
                            continue;
                        }
                        matched_candidates.push(meta.clone());
                    }
                    all_candidates.push(meta);
                }
            }

            log_autopilot(format!(
                "candidates gathered {} matched={} candidates={} elapsed={:.2?}",
                stats.summary(),
                matched_candidates.len(),
                all_candidates.len(),
                scan_start.elapsed()
            ));

            if matched_candidates.is_empty() && all_candidates.is_empty() {
                let _ = tx.send(AutopilotEvent::Finished(total_sent));
                return;
            }

            let remaining_limit = target_limit - total_sent;
            let filtered: Vec<AudioMetadata> = if matched_candidates.len() >= remaining_limit {
                matched_candidates
            } else if !matched_candidates.is_empty() {
                // If library has fewer than target_limit matches,
                // check if this is a small folder (< 20 tracks, e.g. a unit test)
                let total_available = all_candidates.len();
                if total_available <= 10 {
                    matched_candidates
                } else {
                    // In a larger library, keep all matching tracks first,
                    // and supplement remaining slots from diverse candidate tracks
                    let mut pool = matched_candidates;
                    rng.shuffle(&mut all_candidates);
                    for c in all_candidates {
                        if pool.len() >= remaining_limit {
                            break;
                        }
                        if !pool.iter().any(|m| m.filepath == c.filepath) {
                            pool.push(c);
                        }
                    }
                    pool
                }
            } else {
                all_candidates
            };

            let candidate_count = filtered.len();
            let sequence_start = Instant::now();
            let sequenced =
                crate::analysis::mixer::DjMixer::sequence_mix_after(first_sent.as_ref(), filtered, &db, remaining_limit);
            log_autopilot(format!(
                "sequenced {} tracks from candidates={} in {:.2?}",
                sequenced.len(),
                candidate_count,
                sequence_start.elapsed()
            ));

            for track in sequenced {
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                if tx.send(AutopilotEvent::Track(track)).is_ok() {
                    total_sent += 1;
                }
            }

            if !stop_flag.load(Ordering::Relaxed) {
                log_autopilot(format!(
                    "mix complete profile=\"{}\" sent={} total={:.2?}",
                    profile.name(),
                    total_sent,
                    scan_start.elapsed()
                ));
                let _ = tx.send(AutopilotEvent::Finished(total_sent));
            }
            return;
        }

        // 3. DiverseShuffle progressive streaming logic:
        if subdirs.is_empty() {
            if !immediate_files.is_empty() {
                rng.shuffle(&mut immediate_files);
                for f in immediate_files.into_iter().take(target_limit) {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let meta = make_metadata(&f);
                    if tx.send(AutopilotEvent::Track(meta)).is_ok() {
                        total_sent += 1;
                    }
                }
            }
            let _ = tx.send(AutopilotEvent::Finished(total_sent));
            return;
        }

        // If we found immediate files in root, stream a couple right away
        if !immediate_files.is_empty() {
            rng.shuffle(&mut immediate_files);
            let take_count = if subdirs.is_empty() { target_limit } else { 2 };
            for f in immediate_files.iter().take(take_count) {
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                let meta = make_metadata(f);
                if tx.send(AutopilotEvent::Track(meta)).is_ok() {
                    total_sent += 1;
                }
            }
        }

        if subdirs.len() >= target_limit {
            // Large collection: 1 track per subdirectory across diverse artists
            for subdir in &subdirs {
                if stop_flag.load(Ordering::Relaxed) || total_sent >= target_limit {
                    break;
                }
                let mut folder_tracks = Vec::new();
                let mut seen = SeenPaths::default();
                for entry in WalkDir::new(subdir)
                    .max_depth(3)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        let name = e.file_name().to_string_lossy();
                        !name.starts_with('.') && name != "#recycle" && name != "$RECYCLE.BIN" && seen.first_visit(e.path())
                    })
                    .filter_map(|e| e.ok())
                {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let path = entry.path();
                    if path.is_file() && is_supported_audio_file(path) {
                        folder_tracks.push(path.to_path_buf());
                        if folder_tracks.len() >= 10 {
                            break;
                        }
                    }
                }

                if !folder_tracks.is_empty() {
                    let pick = (rng.next_u64() as usize) % folder_tracks.len();
                    let meta = make_metadata(&folder_tracks[pick]);
                    if tx.send(AutopilotEvent::Track(meta)).is_ok() {
                        total_sent += 1;
                    }
                }
            }
        } else {
            // Moderate library (e.g. 5-30 album folders by an artist)
            let per_subdir = target_limit.div_ceil(subdirs.len()).max(1);
            let mut extra_tracks = Vec::new();

            for subdir in &subdirs {
                if stop_flag.load(Ordering::Relaxed) || total_sent >= target_limit {
                    break;
                }
                let mut folder_tracks = Vec::new();
                let mut seen = SeenPaths::default();
                for entry in WalkDir::new(subdir)
                    .max_depth(3)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        let name = e.file_name().to_string_lossy();
                        !name.starts_with('.') && name != "#recycle" && name != "$RECYCLE.BIN" && seen.first_visit(e.path())
                    })
                    .filter_map(|e| e.ok())
                {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let path = entry.path();
                    if path.is_file() && is_supported_audio_file(path) {
                        folder_tracks.push(path.to_path_buf());
                    }
                }

                if !folder_tracks.is_empty() {
                    rng.shuffle(&mut folder_tracks);
                    for (i, p) in folder_tracks.into_iter().enumerate() {
                        if i < per_subdir && total_sent < target_limit {
                            let meta = make_metadata(&p);
                            if tx.send(AutopilotEvent::Track(meta)).is_ok() {
                                total_sent += 1;
                            }
                        } else {
                            extra_tracks.push(p);
                        }
                    }
                }
            }

            // Fill remaining slots
            if total_sent < target_limit && !extra_tracks.is_empty() {
                rng.shuffle(&mut extra_tracks);
                for p in extra_tracks {
                    if stop_flag.load(Ordering::Relaxed) || total_sent >= target_limit {
                        break;
                    }
                    let meta = make_metadata(&p);
                    if tx.send(AutopilotEvent::Track(meta)).is_ok() {
                        total_sent += 1;
                    }
                }
            }
        }

        if !stop_flag.load(Ordering::Relaxed) {
            crate::log_perf!(
                "library",
                "progressive autopilot scan complete ({} tracks) for '{}' -> elapsed: {:.2?}",
                total_sent,
                dir.display(),
                scan_start.elapsed()
            );
            let _ = tx.send(AutopilotEvent::Finished(total_sent));
        }
    }

    /// Background streaming scanner: discovers tracks and streams them in batches of 50 via channel.
    pub fn scan_directory_streaming(
        dir: PathBuf,
        cache: HashMap<PathBuf, AudioMetadata>,
        tx: Sender<Vec<AudioMetadata>>,
        stop_flag: Arc<AtomicBool>,
    ) {
        let scan_start = std::time::Instant::now();
        let mut batch = Vec::new();
        let mut new_discovered = Vec::new();
        let mut full_cache = cache.clone();

        let mut seen = SeenPaths::default();

        for entry in WalkDir::new(&dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !(name.starts_with('.') && name != "." && name != "..") && seen.first_visit(e.path())
            })
            .filter_map(|e| e.ok())
        {
            if stop_flag.load(Ordering::Relaxed) {
                return;
            }

            let path = entry.path();
            if path.is_file() && is_supported_audio_file(path) {
                let file_len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let already_cached = if let Some(cached) = cache.get(path) {
                    cached.file_size == file_len
                } else {
                    false
                };

                if already_cached {
                    continue;
                }

                if let Ok(meta) = extract_metadata(path) {
                    full_cache.insert(meta.filepath.clone(), meta.clone());
                    new_discovered.push(meta.clone());
                    batch.push(meta);

                    if batch.len() >= 50 && tx.send(std::mem::take(&mut batch)).is_err() {
                        return; // receiver dropped
                    }
                }
            }
        }

        if !batch.is_empty() {
            let _ = tx.send(batch);
        }

        if !new_discovered.is_empty() {
            let all_tracks: Vec<AudioMetadata> = full_cache.into_values().collect();
            Self::save_cache(&all_tracks);
        }

        if !stop_flag.load(Ordering::Relaxed) {
            crate::logger::log_raw(
                "PERF",
                "library",
                &format!(
                    "directory streaming scan complete ({} newly indexed) for '{}' -> elapsed: {:.2?}",
                    new_discovered.len(),
                    dir.display(),
                    scan_start.elapsed()
                ),
            );
        }
    }

    /// Run Skim interactive fuzzy search over tracks and return selected items.
    pub fn search_with_skim(tracks: &[AudioMetadata]) -> Vec<AudioMetadata> {
        let track_map: HashMap<PathBuf, AudioMetadata> = tracks
            .iter()
            .map(|t| (t.filepath.clone(), t.clone()))
            .collect();

        let options = SkimOptionsBuilder::default()
            .height(Some("100%"))
            .multi(true)
            .preview(Some(""))
            .prompt(Some("Search Music > "))
            .build()
            .unwrap_or_default();

        let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();
        for track in tracks {
            let _ = tx.send(Arc::new(SkimTrackItem {
                metadata: track.clone(),
            }));
        }
        drop(tx);

        if let Some(out) = Skim::run_with(&options, Some(rx)) {
            if !out.is_abort {
                return out
                    .selected_items
                    .iter()
                    .filter_map(|item| {
                        let path = PathBuf::from(item.output().to_string());
                        track_map.get(&path).cloned()
                    })
                    .collect();
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skim_track_item_formatting() {
        let meta = AudioMetadata {
            filepath: PathBuf::from("/music/song.mp3"),
            filename: "song.mp3".to_string(),
            title: "Rock Anthem".to_string(),
            artist: "Band".to_string(),
            album: "Hits".to_string(),
            genre: "Rock".to_string(),
            format_name: "MP3".to_string(),
            duration_seconds: 180.0,
            file_size: 5000,
            bitrate_kbps: 320,
            sample_rate_hz: 44100,
            channels: 2,
        };

        let item = SkimTrackItem { metadata: meta };
        assert_eq!(item.text(), "Band - Rock Anthem [MP3]");
        assert_eq!(item.output(), "/music/song.mp3");
    }
}
