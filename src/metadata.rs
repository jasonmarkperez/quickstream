//! Native audio metadata extraction wrapping lofty (pure Rust, zero subprocesses).

use lofty::config::ParseOptions;
use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
            format!("[{}] {}{}{}", ext, artist_part, self.display_title(), time_part)
        } else {
            self.filename.clone()
        }
    }

    /// Format label for playback queue display without format/filetype badge.
    pub fn format_queue_label(&self, metadata_mode: bool) -> String {
        let time_part = if self.duration_seconds > 0.0 {
            format!(" ({})", self.formatted_duration())
        } else {
            String::new()
        };
        if metadata_mode {
            let artist_part = if !self.artist.is_empty() {
                format!("{} - ", self.artist)
            } else {
                String::new()
            };
            format!("{}{}{}", artist_part, self.display_title(), time_part)
        } else {
            format!("{}{}", self.filename, time_part)
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

pub fn get_audio_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" | "wave" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        _ => "application/octet-stream",
    }
}

/// Derives intelligent heuristic title, artist, and album from file and folder paths
/// without reading tags from disk, ensuring instant zero-cost initial display.
pub fn parse_path_heuristics(path: &Path) -> (String, String, String) {
    let raw_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut title = raw_stem.trim().to_string();
    let mut artist = String::new();
    let mut album = String::new();

    // 1. Strip leading track number prefixes (e.g. "01-08 ", "01 - ", "03 ", "01. ", "01_")
    let trimmed = title.as_str();
    let digit_count = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_count > 0 {
        let after_digits = &trimmed[digit_count..];
        if after_digits.starts_with('-')
            || after_digits.starts_with('.')
            || after_digits.starts_with('_')
            || after_digits.starts_with(' ')
        {
            let mut rest = &after_digits[1..];
            let digit_count2 = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digit_count2 > 0 {
                rest = &rest[digit_count2..];
            }
            rest = rest.trim_start_matches(|c: char| c == '-' || c == '.' || c == '_' || c.is_whitespace());
            if !rest.is_empty() {
                title = rest.to_string();
            }
        }
    }

    // 2. Check if the remaining title contains " - " (e.g. "Artist - Title" or "Album - Artist - Title")
    if title.contains(" - ") {
        let parts: Vec<&str> = title.split(" - ").map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
        if parts.len() == 2 {
            artist = parts[0].to_string();
            title = parts[1].to_string();
        } else if parts.len() >= 3 {
            album = parts[0].to_string();
            artist = parts[1].to_string();
            title = parts[2..].join(" - ");
        }
    } else if title.contains("- ") {
        let parts: Vec<&str> = title.split("- ").map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
        if parts.len() == 2 {
            artist = parts[0].to_string();
            title = parts[1].to_string();
        }
    }

    // 3. Inspect directory hierarchy for artist and album
    if let Some(parent) = path.parent() {
        let parent_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_root_like = |n: &str| {
            n.is_empty()
                || n == "/"
                || n == "Volumes"
                || n == "Users"
                || n == "home"
                || n == "Music"
                || n == "var"
                || n == "tmp"
        };

        if let Some(grandparent) = parent.parent() {
            let gp_name = grandparent.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !is_root_like(gp_name) && !is_root_like(parent_name) {
                // If grandparent folder name is a full artist name, prefer it
                if artist.is_empty() || gp_name.to_lowercase().contains(&artist.to_lowercase()) {
                    artist = gp_name.to_string();
                }
                if album.is_empty() {
                    album = parent_name.to_string();
                }
            } else if !is_root_like(parent_name) && artist.is_empty() {
                artist = parent_name.to_string();
            }
        } else if !is_root_like(parent_name) && artist.is_empty() {
            artist = parent_name.to_string();
        }
    }

    (title, artist, album)
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

    let file_size_mb = file_size as f64 / 1_048_576.0;
    let _timer = crate::log_perf!("metadata", "extract_metadata '{}' ({:.2} MB)", filename, file_size_mb);

    // Probe and parse with lofty, skipping cover art to avoid multi-megabyte image reads
    let parse_opts = ParseOptions::new().read_cover_art(false);
    let tagged_file = Probe::open(path)?
        .options(parse_opts)
        .guess_file_type()?
        .read()?;
    let properties = tagged_file.properties();

    let mut duration_seconds = properties.duration().as_secs_f64();
    let bitrate_kbps = properties.audio_bitrate().unwrap_or(0);
    let sample_rate_hz = properties.sample_rate().unwrap_or(0);
    let channels = properties.channels().unwrap_or(2);

    // If duration couldn't be determined from headers, estimate from bitrate and file size
    if duration_seconds <= 0.0 && bitrate_kbps > 0 && file_size > 0 {
        duration_seconds = (file_size as f64 * 8.0) / (bitrate_kbps as f64 * 1000.0);
    }

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

    let (h_title, h_artist, h_album) = parse_path_heuristics(path);

    let display_title = if !title.is_empty() {
        title
    } else if !h_title.is_empty() {
        h_title
    } else {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(filename.clone())
    };

    let final_artist = if !artist.is_empty() {
        artist
    } else {
        h_artist
    };

    let final_album = if !album.is_empty() {
        album
    } else {
        h_album
    };

    Ok(AudioMetadata {
        filepath: path.to_path_buf(),
        filename,
        title: display_title,
        artist: final_artist,
        album: final_album,
        genre,
        format_name: ext,
        duration_seconds,
        file_size,
        bitrate_kbps,
        sample_rate_hz,
        channels,
    })
}

/// Create fast fallback metadata directly from filepath and file size without reading tags.
pub fn fallback_metadata(path: &Path) -> AudioMetadata {
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    let title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.clone());
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("UNKNOWN")
        .to_uppercase();
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let duration_seconds = if file_size > 0 {
        (file_size as f64 * 8.0) / (320.0 * 1000.0)
    } else {
        0.0
    };

    AudioMetadata {
        filepath: path.to_path_buf(),
        filename,
        title,
        artist: String::new(),
        album: String::new(),
        genre: String::new(),
        format_name: ext,
        duration_seconds,
        file_size,
        bitrate_kbps: 320,
        sample_rate_hz: 44100,
        channels: 2,
    }
}

/// Extract cover artwork from an audio file.
/// Looks first for embedded ID3/FLAC/Vorbis pictures, then scans the folder for image files.
/// Returns (image_bytes, mime_type).
pub fn extract_cover_art(path: &Path) -> Option<(Vec<u8>, String)> {
    if !path.exists() {
        return None;
    }

    // 1. Try embedded tags via lofty
    if let Ok(probe) = Probe::open(path) {
        if let Ok(guessed) = probe.guess_file_type() {
            if let Ok(tagged_file) = guessed.read() {
                let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag());
                if let Some(tag) = tag {
                    let pics = tag.pictures();
                    let pic = pics
                        .iter()
                        .find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront)
                        .or_else(|| pics.first());
                    if let Some(p) = pic {
                        let mime = p
                            .mime_type()
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_else(|| "image/jpeg".to_string());
                        return Some((p.data().to_vec(), mime));
                    }
                }
            }
        }
    }

    // 2. Fall back to image files in the same directory
    if let Some(parent) = path.parent() {
        let candidates = [
            "cover.jpg", "cover.jpeg", "cover.png", "cover.webp",
            "folder.jpg", "folder.jpeg", "folder.png",
            "front.jpg", "front.jpeg", "front.png",
            "album.jpg", "album.jpeg", "album.png",
            "artwork.jpg", "artwork.png",
        ];

        for name in &candidates {
            let candidate_path = parent.join(name);
            if candidate_path.is_file() {
                if let Ok(data) = std::fs::read(&candidate_path) {
                    let mime = if candidate_path
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .as_deref()
                        == Some("png")
                    {
                        "image/png"
                    } else if candidate_path
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .as_deref()
                        == Some("webp")
                    {
                        "image/webp"
                    } else {
                        "image/jpeg"
                    };
                    return Some((data, mime.to_string()));
                }
            }
        }
    }

    None
}

/// Fallback SVG placeholder when no cover art is available.
pub fn default_cover_svg() -> &'static str {
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 500 500" width="100%" height="100%">
  <defs>
    <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#141824"/>
      <stop offset="50%" stop-color="#0f111a"/>
      <stop offset="100%" stop-color="#090a0f"/>
    </linearGradient>
    <linearGradient id="vinyl" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#2a2e3d"/>
      <stop offset="100%" stop-color="#161822"/>
    </linearGradient>
    <linearGradient id="accent" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#00d2ff"/>
      <stop offset="100%" stop-color="#0072ff"/>
    </linearGradient>
  </defs>
  <rect width="500" height="500" rx="24" fill="url(#bg)"/>
  <circle cx="250" cy="250" r="180" fill="url(#vinyl)" stroke="#3a4055" stroke-width="4"/>
  <circle cx="250" cy="250" r="145" fill="none" stroke="#252938" stroke-width="2"/>
  <circle cx="250" cy="250" r="115" fill="none" stroke="#222634" stroke-width="2"/>
  <circle cx="250" cy="250" r="85" fill="none" stroke="#1f222e" stroke-width="2"/>
  <circle cx="250" cy="250" r="60" fill="url(#accent)"/>
  <circle cx="250" cy="250" r="18" fill="#090a0f"/>
  <path d="M242 220 L270 220 L270 265 A16 16 0 1 1 254 249 L254 232 L242 232 Z" fill="#ffffff" opacity="0.9"/>
</svg>"##
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
            "[FLAC] Test Artist - Test Song (02:05)"
        );
        assert_eq!(meta.format_display_label(false), "song.flac");

        assert_eq!(
            meta.format_queue_label(true),
            "Test Artist - Test Song (02:05)"
        );
        assert_eq!(meta.format_queue_label(false), "song.flac (02:05)");
    }

    #[test]
    fn test_default_cover_svg() {
        let svg = default_cover_svg();
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("vinyl"));
    }

    #[test]
    fn test_extract_cover_art_from_folder() {
        let temp_dir = std::env::temp_dir().join(format!("quickstream_art_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let track_path = temp_dir.join("track01.wav");
        std::fs::write(&track_path, b"RIFF dummy").unwrap();

        // No cover yet
        assert_eq!(extract_cover_art(&track_path), None);

        // Put cover.jpg in the same directory
        let cover_path = temp_dir.join("cover.jpg");
        std::fs::write(&cover_path, b"fake-jpg-data").unwrap();

        let art = extract_cover_art(&track_path);
        assert!(art.is_some());
        let (bytes, mime) = art.unwrap();
        assert_eq!(bytes, b"fake-jpg-data");
        assert_eq!(mime, "image/jpeg");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
