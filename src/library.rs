//! Library directory indexing (walkdir), JSONL caching, and Skim fuzzy search integration.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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
        let mut tracks = Vec::new();
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() && is_supported_audio_file(path) {
                if let Ok(meta) = extract_metadata(path) {
                    tracks.push(meta);
                }
            }
        }
        tracks
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
