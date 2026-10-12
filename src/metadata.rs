//! Native audio metadata extraction wrapping lofty (pure Rust, zero subprocesses).

use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioMetadata {
    pub filepath: PathBuf,
    pub filename: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub format_name: String,
    pub duration_seconds: f64,
    pub file_size: u64,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub channels: u8,
}

impl AudioMetadata {
    pub fn display_title(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else {
            &self.filename
        }
    }

    pub fn formatted_duration(&self) -> String {
        let total = self.duration_seconds.round() as u64;
        let mins = total / 60;
        let secs = total % 60;
        format!("{:02}:{:02}", mins, secs)
    }

    pub fn format_display_label(&self, metadata_mode: bool) -> String {
        let ext = self.format_name.to_uppercase();
        if metadata_mode {
            let artist_part = if !self.artist.is_empty() {
                format!("{} - ", self.artist)
            } else {
                String::new()
            };
            let time_part = if self.duration_seconds > 0.0 {
                format!(" ({})", self.formatted_duration())
            } else {
                String::new()
            };
            format!("🎵 [{}] {}{}{}", ext, artist_part, self.display_title(), time_part)
        } else {
            format!("🎵 [{}] {}", ext, self.filename)
        }
    }
}

pub fn is_supported_audio_file(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        matches!(
            ext.to_lowercase().as_str(),
            "mp3" | "flac" | "wav" | "wave" | "aiff" | "aif" | "ogg" | "opus" | "m4a" | "aac"
        )
    } else {
        false
    }
}

pub fn extract_metadata(path: &Path) -> anyhow::Result<AudioMetadata> {
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("UNKNOWN")
        .to_uppercase();

    // Probe and parse with lofty
    let tagged_file = Probe::open(path)?.guess_file_type()?.read()?;
    let properties = tagged_file.properties();

    let duration_seconds = properties.duration().as_secs_f64();
    let bitrate_kbps = properties.audio_bitrate().unwrap_or(0);
    let sample_rate_hz = properties.sample_rate().unwrap_or(0);
    let channels = properties.channels().unwrap_or(2);

    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag());

    let title = tag
        .and_then(|t| t.title().map(|s| s.to_string()))
        .unwrap_or_default();

    let artist = tag
        .and_then(|t| t.artist().map(|s| s.to_string()))
        .unwrap_or_default();

    let album = tag
        .and_then(|t| t.album().map(|s| s.to_string()))
        .unwrap_or_default();

    let genre = tag
        .and_then(|t| t.genre().map(|s| s.to_string()))
        .unwrap_or_default();

    let display_title = if !title.is_empty() {
        title
    } else {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(filename.clone())
    };

    Ok(AudioMetadata {
        filepath: path.to_path_buf(),
        filename,
        title: display_title,
        artist,
        album,
        genre,
        format_name: ext,
        duration_seconds,
        file_size,
        bitrate_kbps,
        sample_rate_hz,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_supported_audio_file() {
        assert!(is_supported_audio_file(Path::new("song.mp3")));
        assert!(is_supported_audio_file(Path::new("song.flac")));
        assert!(is_supported_audio_file(Path::new("song.wav")));
        assert!(is_supported_audio_file(Path::new("song.aiff")));
        assert!(is_supported_audio_file(Path::new("song.ogg")));
        assert!(!is_supported_audio_file(Path::new("document.txt")));
        assert!(!is_supported_audio_file(Path::new("image.png")));
    }

    #[test]
    fn test_format_label() {
        let meta = AudioMetadata {
            filepath: PathBuf::from("/music/song.flac"),
            filename: "song.flac".to_string(),
            title: "Test Song".to_string(),
            artist: "Test Artist".to_string(),
            album: "Test Album".to_string(),
            genre: "Rock".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 125.0,
            file_size: 1048576,
            bitrate_kbps: 1411,
            sample_rate_hz: 44100,
            channels: 2,
        };

        assert_eq!(meta.formatted_duration(), "02:05");
        assert_eq!(
            meta.format_display_label(true),
            "🎵 [FLAC] Test Artist - Test Song (02:05)"
        );
        assert_eq!(meta.format_display_label(false), "🎵 [FLAC] song.flac");
    }
}
