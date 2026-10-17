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

#[test]
fn test_multi_token_out_of_order_search() {
    let (root, tracks) = sample_tracks();
    let mut modal = SearchModalState::new(root, tracks);

    // Out-of-order query: "android radiohead" (words reversed compared to "Radiohead - Paranoid Android")
    modal.update_query("android radiohead".to_string());
    assert!(!modal.filtered.is_empty(), "Should match regardless of word order");
    assert_eq!(modal.selected_track().unwrap().artist, "Radiohead");
    assert_eq!(modal.selected_track().unwrap().title, "Paranoid Android");

    // Out-of-order query: "blue miles"
    modal.update_query("blue miles".to_string());
    assert!(!modal.filtered.is_empty());
    assert_eq!(modal.selected_track().unwrap().artist, "Miles Davis");
}

#[test]
fn test_background_streaming_scanner() {
    let root = PathBuf::from("/music");
    let (tx, rx) = std::sync::mpsc::channel();
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut modal = SearchModalState::new_with_scanner(
        root.clone(),
        Vec::new(),
        Some(rx),
        Some(stop_flag),
    );

    assert!(modal.is_scanning);
    assert_eq!(modal.all_tracks.len(), 0);

    // Stream first batch
    let batch1 = vec![
        AudioMetadata {
            filepath: root.join("track1.flac"),
            filename: "track1.flac".to_string(),
            title: "Track 1".to_string(),
            artist: "Artist A".to_string(),
            album: "Album A".to_string(),
            genre: "Rock".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 180.0,
            file_size: 1000,
            bitrate_kbps: 900,
            sample_rate_hz: 44100,
            channels: 2,
        },
    ];
    tx.send(batch1).unwrap();

    modal.poll_background_scan();
    assert_eq!(modal.all_tracks.len(), 1);
    assert!(modal.is_scanning);

    // Stream second batch and close sender
    let batch2 = vec![
        AudioMetadata {
            filepath: root.join("track2.flac"),
            filename: "track2.flac".to_string(),
            title: "Track 2".to_string(),
            artist: "Artist B".to_string(),
            album: "Album B".to_string(),
            genre: "Jazz".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 240.0,
            file_size: 1500,
            bitrate_kbps: 900,
            sample_rate_hz: 44100,
            channels: 2,
        },
    ];
    tx.send(batch2).unwrap();
    drop(tx);

    modal.poll_background_scan();
    assert_eq!(modal.all_tracks.len(), 2);
    assert!(!modal.is_scanning, "Scanner should be marked complete when channel disconnects");
}

#[test]
fn test_large_library_search_performance() {
    let root = PathBuf::from("/large_library");
    let mut large_tracks = Vec::with_capacity(20_000);

    // Generate 20,000 synthetic tracks
    for i in 0..20_000 {
        large_tracks.push(AudioMetadata {
            filepath: root.join(format!("artist_{:04}/album_{:03}/track_{:05}.flac", i % 100, i % 500, i)),
            filename: format!("track_{:05}.flac", i),
            title: format!("Song Title {:05}", i),
            artist: format!("Popular Artist {:03}", i % 200),
            album: format!("Studio Album {:03}", i % 500),
            genre: "Electronic".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 210.0,
            file_size: 30_000_000,
            bitrate_kbps: 1000,
            sample_rate_hz: 44100,
            channels: 2,
        });
    }

    let start_build = std::time::Instant::now();
    let mut modal = SearchModalState::new(root, large_tracks);
    let build_elapsed = start_build.elapsed();
    println!("Indexing 20,000 tracks took: {:?}", build_elapsed);

    // Benchmark search filter execution across 20,000 tracks
    let start_search = std::time::Instant::now();
    modal.update_query("popular 042 song 150".to_string());
    let search_elapsed = start_search.elapsed();
    println!("Fuzzy query across 20,000 tracks took: {:?}", search_elapsed);

    assert!(!modal.filtered.is_empty());
    assert!(
        search_elapsed.as_millis() < 250,
        "Search across 20,000 tracks must be fast (<250ms in unoptimized debug, ~11ms in release), took {:?}",
        search_elapsed
    );
}

#[test]
fn test_flutter_vs_future_sound_of_london() {
    let root = PathBuf::from("/Volumes/Music");
    let tracks = vec![
        AudioMetadata {
            filepath: root.join("The Future Sound Of London/FSOL - Multiple Angled Distortions/THE FUTURE SOUND OF LONDON - Multiple Angled Distortions - 04 C-Lab-Anim.flac"),
            filename: "THE FUTURE SOUND OF LONDON - Multiple Angled Distortions - 04 C-Lab-Anim.flac".to_string(),
            title: "C-Lab-Anim".to_string(),
            artist: "The Future Sound of London".to_string(),
            album: "Multiple Angled Distortions".to_string(),
            genre: "Electronic".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 300.0,
            file_size: 1000,
            bitrate_kbps: 900,
            sample_rate_hz: 44100,
            channels: 2,
        },
        AudioMetadata {
            filepath: root.join("Autechre/Anti Ep/03 Flutter.flac"),
            filename: "03 Flutter.flac".to_string(),
            title: "Flutter".to_string(),
            artist: "Autechre".to_string(),
            album: "Anti Ep".to_string(),
            genre: "Electronic".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 600.0,
            file_size: 1000,
            bitrate_kbps: 900,
            sample_rate_hz: 44100,
            channels: 2,
        },
    ];

    let mut modal = SearchModalState::new(root, tracks);
    modal.update_query("flutter".to_string());

    // Only Autechre - Flutter should match! The Future Sound of London should NOT match!
    assert_eq!(modal.filtered.len(), 1, "Should strictly match Flutter and exclude Future Sound of London");
    assert_eq!(modal.selected_track().unwrap().artist, "Autechre");
    assert_eq!(modal.selected_track().unwrap().title, "Flutter");
}

#[test]
fn test_search_modal_paging_and_jumping() {
    let root = PathBuf::from("/music");
    let tracks: Vec<AudioMetadata> = (0..30)
        .map(|i| AudioMetadata {
            filepath: root.join(format!("track_{:02}.flac", i)),
            filename: format!("track_{:02}.flac", i),
            title: format!("Track {:02}", i),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            genre: "Rock".to_string(),
            format_name: "FLAC".to_string(),
            duration_seconds: 120.0,
            file_size: 1000,
            bitrate_kbps: 900,
            sample_rate_hz: 44100,
            channels: 2,
        })
        .collect();

    let mut modal = SearchModalState::new(root, tracks);
    assert_eq!(modal.filtered.len(), 30);
    assert_eq!(modal.selected_index, 0);

    // Page down by 10
    modal.page_down(10);
    assert_eq!(modal.selected_index, 10);

    // Page down by 10 again
    modal.page_down(10);
    assert_eq!(modal.selected_index, 20);

    // End jumps to 29
    modal.end();
    assert_eq!(modal.selected_index, 29);

    // Page down at last item wraps to 0
    modal.page_down(10);
    assert_eq!(modal.selected_index, 0);

    // Page up at 0 wraps to 29
    modal.page_up(10);
    assert_eq!(modal.selected_index, 29);

    // Page up by 10
    modal.page_up(10);
    assert_eq!(modal.selected_index, 19);

    // Home jumps to 0
    modal.home();
    assert_eq!(modal.selected_index, 0);
}




fn streamed_track(root: &Path, i: usize) -> AudioMetadata {
    AudioMetadata {
        filepath: root.join(format!("track{}.flac", i)),
        filename: format!("track{}.flac", i),
        title: format!("Track {}", i),
        artist: "Artist".to_string(),
        album: "Album".to_string(),
        genre: "Rock".to_string(),
        format_name: "FLAC".to_string(),
        duration_seconds: 180.0,
        file_size: 1000,
        bitrate_kbps: 900,
        sample_rate_hz: 44100,
        channels: 2,
    }
}

#[test]
fn test_streaming_batches_coalesce_filter_updates() {
    let root = PathBuf::from("/music");
    let (tx, rx) = std::sync::mpsc::channel();
    let mut modal = SearchModalState::new_with_scanner(root.clone(), Vec::new(), Some(rx), None);

    tx.send(vec![streamed_track(&root, 1)]).unwrap();
    modal.poll_background_scan();
    assert_eq!(modal.filtered.len(), 1, "first batch is shown immediately");

    // A batch arriving right after a filter pass is indexed but not re-filtered yet.
    tx.send(vec![streamed_track(&root, 2)]).unwrap();
    modal.poll_background_scan();
    assert_eq!(modal.all_tracks.len(), 2);
    assert_eq!(modal.filtered.len(), 1);

    // Once the coalescing interval passes, the next poll applies the filter.
    std::thread::sleep(std::time::Duration::from_millis(300));
    modal.poll_background_scan();
    assert_eq!(modal.filtered.len(), 2);

    // Scan completion applies the filter immediately.
    tx.send(vec![streamed_track(&root, 3)]).unwrap();
    drop(tx);
    modal.poll_background_scan();
    assert_eq!(modal.filtered.len(), 3);

    // Typing always filters immediately.
    modal.append_char('3');
    assert_eq!(modal.filtered.len(), 1);
}
