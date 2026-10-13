use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use quickstream::library::LibraryManager;
use quickstream::metadata::AudioMetadata;
use quickstream::ui::search::SearchModalState;

fn create_dummy_audio_file(path: &Path) {
    let mut file = File::create(path).unwrap();
    let sample_rate: u32 = 44100;
    let channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * ((bits_per_sample as u32) / 8);
    let block_align = channels * (bits_per_sample / 8);
    let num_samples: u32 = 44100;
    let data_size = num_samples * (block_align as u32);

    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();
    file.write_all(&vec![0u8; data_size as usize]).unwrap();
}

fn sample_tracks() -> (PathBuf, Vec<AudioMetadata>) {
    let root = PathBuf::from("/music");
    let tracks = vec![
        AudioMetadata {
            filepath: root.join("Radiohead/OK Computer/02 - Paranoid Android.flac"),
            filename: "02 - Paranoid Android.flac".to_string(),
            title: "Paranoid Android".to_string(),
            artist: "Radiohead".to_string(),
            album: "OK Computer".to_string(),
            genre: "Alternative Rock".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 383.0,
            file_size: 45_000_000,
            bitrate_kbps: 940,
            sample_rate_hz: 44100,
            channels: 2,
        },
        AudioMetadata {
            filepath: root.join("Daft Punk/Discovery/04 - Harder Better Faster Stronger.flac"),
            filename: "04 - Harder Better Faster Stronger.flac".to_string(),
            title: "Harder Better Faster Stronger".to_string(),
            artist: "Daft Punk".to_string(),
            album: "Discovery".to_string(),
            genre: "Electronic".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 224.0,
            file_size: 28_000_000,
            bitrate_kbps: 998,
            sample_rate_hz: 44100,
            channels: 2,
        },
        AudioMetadata {
            filepath: root.join("Miles Davis/Kind of Blue/01 - So What.mp3"),
            filename: "01 - So What.mp3".to_string(),
            title: "So What".to_string(),
            artist: "Miles Davis".to_string(),
            album: "Kind of Blue".to_string(),
            genre: "Jazz".to_string(),
            format_name: "MP3".to_string(),
            duration_seconds: 562.0,
            file_size: 22_500_000,
            bitrate_kbps: 320,
            sample_rate_hz: 44100,
            channels: 2,
        },
        AudioMetadata {
            filepath: root.join("Ambient/Aphex Twin/Pulsewidth.wav"),
            filename: "Pulsewidth.wav".to_string(),
            title: "Pulsewidth".to_string(),
            artist: "Aphex Twin".to_string(),
            album: "Selected Ambient Works 85-92".to_string(),
            genre: "IDM".to_string(),
            format_name: "WAV".to_string(),
            duration_seconds: 226.0,
            file_size: 39_900_000,
            bitrate_kbps: 1411,
            sample_rate_hz: 44100,
            channels: 2,
        },
    ];
    (root, tracks)
}

#[test]
fn test_fuzzy_search_multi_field_matching() {
    let (root, tracks) = sample_tracks();
    let mut modal = SearchModalState::new(root, tracks);

    // Initial state with empty query: all tracks returned, sorted by artist
    assert_eq!(modal.filtered.len(), 4);
    assert_eq!(modal.selected_track().unwrap().artist, "Aphex Twin");

    // 1. Search by artist: "daft"
    modal.update_query("daft".to_string());
    assert!(!modal.filtered.is_empty());
    assert_eq!(modal.selected_track().unwrap().artist, "Daft Punk");
    assert_eq!(modal.selected_track().unwrap().title, "Harder Better Faster Stronger");

    // 2. Search by album: "kind of blue"
    modal.update_query("kind of blue".to_string());
    assert!(!modal.filtered.is_empty());
    assert_eq!(modal.selected_track().unwrap().artist, "Miles Davis");

    // 3. Search by title: "paranoid"
    modal.update_query("paranoid".to_string());
    assert!(!modal.filtered.is_empty());
    assert_eq!(modal.selected_track().unwrap().title, "Paranoid Android");

    // 4. Search by relative path: "Ambient/Aphex"
    modal.update_query("Ambient/Aphex".to_string());
    assert!(!modal.filtered.is_empty());
    assert_eq!(modal.selected_track().unwrap().artist, "Aphex Twin");

    // 5. Query matching multiple tracks: "flac" (file format/extension)
    modal.update_query("flac".to_string());
    assert!(modal.filtered.len() >= 2);
    let matched_artists: Vec<String> = modal.filtered.iter().map(|(_, t)| t.artist.clone()).collect();
    assert!(matched_artists.contains(&"Radiohead".to_string()));
    assert!(matched_artists.contains(&"Daft Punk".to_string()));
}

#[test]
fn test_fuzzy_search_query_edits_and_navigation() {
    let (root, tracks) = sample_tracks();
    let mut modal = SearchModalState::new(root, tracks);

    // Typing character by character
    modal.append_char('r');
    modal.append_char('a');
    modal.append_char('d');
    assert_eq!(modal.query, "rad");
    assert!(!modal.filtered.is_empty());
    assert_eq!(modal.selected_track().unwrap().artist, "Radiohead");

    // Backspace
    modal.backspace();
    modal.backspace();
    modal.backspace();
    assert_eq!(modal.query, "");
    assert_eq!(modal.filtered.len(), 4);

    // Navigation and wraparound
    assert_eq!(modal.selected_index, 0);
    modal.move_selection_up(); // Should wrap around to last (index 3)
    assert_eq!(modal.selected_index, 3);
    modal.move_selection_down(); // Should wrap to 0
    assert_eq!(modal.selected_index, 0);
    modal.move_selection_down(); // Move to index 1
    assert_eq!(modal.selected_index, 1);

    // Batch retrieval
    let all = modal.all_filtered_tracks();
    assert_eq!(all.len(), 4);
}

#[test]
fn test_scoped_directory_scan() {
    let temp_root = std::env::temp_dir().join(format!("quickstream_scan_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_root);
    fs::create_dir_all(&temp_root).unwrap();

    let sub_a = temp_root.join("Artist A/Album 1");
    let sub_b = temp_root.join("Artist B/Album 2");
    let hidden = temp_root.join(".git/objects");
    fs::create_dir_all(&sub_a).unwrap();
    fs::create_dir_all(&sub_b).unwrap();
    fs::create_dir_all(&hidden).unwrap();

    // Create audio files in visible subdirectories
    let f1 = sub_a.join("track1.wav");
    let f2 = sub_b.join("track2.wav");
    let f_hidden = hidden.join("fake_track.wav");
    create_dummy_audio_file(&f1);
    create_dummy_audio_file(&f2);
    create_dummy_audio_file(&f_hidden);

    // Non-audio file
    let txt = sub_a.join("notes.txt");
    fs::write(&txt, b"liner notes").unwrap();

    let scanned = LibraryManager::scan_directory_scoped(&temp_root);
    assert_eq!(scanned.len(), 2, "Should find exactly 2 tracks, ignoring hidden and non-audio files");

    let filenames: Vec<String> = scanned.iter().map(|t| t.filename.clone()).collect();
    assert!(filenames.contains(&"track1.wav".to_string()));
    assert!(filenames.contains(&"track2.wav".to_string()));
    assert!(!filenames.contains(&"fake_track.wav".to_string()));

    let _ = fs::remove_dir_all(&temp_root);
}
