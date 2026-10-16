//! Library directory indexing (walkdir), JSONL caching, and Skim fuzzy search integration.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
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
                        true
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
                        true
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

    /// Progressive background scanner for Autopilot mode:
    /// Streams discovered tracks one-by-one over an mpsc channel,
    /// enabling playback to start within milliseconds without blocking the UI thread.
    pub fn scan_directory_autopilot_progressive(
        dir: PathBuf,
        known_subdirs: Vec<PathBuf>,
        cache: HashMap<PathBuf, AudioMetadata>,
        tx: Sender<AutopilotEvent>,
        stop_flag: Arc<AtomicBool>,
    ) {
        let scan_start = std::time::Instant::now();
        let target_limit = 60;
        let mut total_sent = 0;
        let mut rng = crate::queue::FastRng::new();

        let make_metadata = |path: &Path| -> AudioMetadata {
            let file_len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if let Some(cached) = cache.get(path) {
                if cached.file_size == file_len && !cached.title.is_empty() {
                    return cached.clone();
                }
            }
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

        // 1. Determine subdirectories:
        // If known_subdirs already contains plenty of subdirectories (e.g. from the active browser),
        // we reuse them immediately to avoid an expensive fs::read_dir call over the network!
        let mut subdirs = if known_subdirs.len() >= target_limit {
            known_subdirs
        } else {
            let mut found_subdirs = Vec::new();
            let mut immediate_files = Vec::new();

            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
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
                        immediate_files.push(path);
                    }
                }
            }

            // If we found immediate files, stream a few right away
            if !immediate_files.is_empty() {
                rng.shuffle(&mut immediate_files);
                let take_count = if found_subdirs.is_empty() { target_limit } else { 2 };
                for f in immediate_files.iter().take(take_count) {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let meta = make_metadata(f);
                    if tx.send(AutopilotEvent::Track(meta)).is_ok() {
                        total_sent += 1;
                    }
                }
                if found_subdirs.is_empty() {
                    let _ = tx.send(AutopilotEvent::Finished(total_sent));
                    return;
                }
            }

            found_subdirs
        };

        if subdirs.is_empty() {
            let _ = tx.send(AutopilotEvent::Finished(total_sent));
            return;
        }

        // Shuffle subdirectories for diverse library sampling
        rng.shuffle(&mut subdirs);

        if subdirs.len() >= target_limit {
            // Large collection: 1 track per subdirectory across diverse artists
            for subdir in &subdirs {
                if stop_flag.load(Ordering::Relaxed) || total_sent >= target_limit {
                    break;
                }
                let mut folder_tracks = Vec::new();
                for entry in WalkDir::new(subdir)
                    .max_depth(3)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        let name = e.file_name().to_string_lossy();
                        !name.starts_with('.') && name != "#recycle" && name != "$RECYCLE.BIN"
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
                for entry in WalkDir::new(subdir)
                    .max_depth(3)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        let name = e.file_name().to_string_lossy();
                        !name.starts_with('.') && name != "#recycle" && name != "$RECYCLE.BIN"
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

        for entry in WalkDir::new(&dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !(name.starts_with('.') && name != "." && name != "..")
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
