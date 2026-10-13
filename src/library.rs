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
                    if track.filepath.exists() {
                        tracks.push(track);
                    }
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

    pub fn scan_directory_scoped_with_cache(
        dir: &Path,
        cache: &HashMap<PathBuf, AudioMetadata>,
    ) -> Vec<AudioMetadata> {
        let mut tracks = Vec::new();
        let mut new_discovered = false;
        let mut full_cache = cache.clone();

        for entry in WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                if name.starts_with('.') && name != "." && name != ".." {
                    return false;
                }
                true
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() && is_supported_audio_file(path) {
                let file_len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if let Some(cached) = cache.get(path) {
                    if cached.file_size == file_len {
                        tracks.push(cached.clone());
                        continue;
                    }
                }
                if let Ok(meta) = extract_metadata(path) {
                    tracks.push(meta.clone());
                    full_cache.insert(meta.filepath.clone(), meta);
                    new_discovered = true;
                }
            }
        }

        if new_discovered {
            let all_tracks: Vec<AudioMetadata> = full_cache.into_values().collect();
            Self::save_cache(&all_tracks);
        }

        tracks
    }

    /// Background streaming scanner: discovers tracks and streams them in batches of 50 via channel.
    pub fn scan_directory_streaming(
        dir: PathBuf,
        cache: HashMap<PathBuf, AudioMetadata>,
        tx: Sender<Vec<AudioMetadata>>,
        stop_flag: Arc<AtomicBool>,
    ) {
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
