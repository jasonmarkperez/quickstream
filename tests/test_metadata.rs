mod common;

use std::fs::{self, File};
use std::io::Write;

use quickstream::analysis::{AnalysisDatabase, MixProfile, TrackFeatures};
use quickstream::library::{AutopilotEvent, LibraryManager, ProfileScanLimits};
use quickstream::metadata::{extract_metadata, is_supported_audio_file};

fn create_dummy_wav(path: &std::path::Path) {
    let mut file = File::create(path).unwrap();
    let sample_rate: u32 = 44100;
    let channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * ((bits_per_sample as u32) / 8);
    let block_align = channels * (bits_per_sample / 8);
    let num_samples: u32 = 44100 * 2; // 2 seconds
    let data_size = num_samples * (block_align as u32);

    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();
    file.write_all(&vec![0u8; data_size as usize]).unwrap();
}

#[test]
fn test_extract_metadata_real_audio() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_meta_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let wav_file = temp_dir.join("test_audio.wav");
    create_dummy_wav(&wav_file);

    assert!(is_supported_audio_file(&wav_file));

    let meta = extract_metadata(&wav_file).expect("Should extract metadata from valid WAV file");
    assert_eq!(meta.format_name, "WAV");
    assert_eq!(meta.channels, 2);
    assert_eq!(meta.sample_rate_hz, 44100);
    assert!((meta.duration_seconds - 2.0).abs() < 0.1);
    assert_eq!(meta.formatted_duration(), "00:02");
    let long = quickstream::metadata::AudioMetadata {
        duration_seconds: 4500.0,
        ..Default::default()
    };
    assert_eq!(long.formatted_duration(), "1:15:00", "the TUI's one duration format");

    // Display labels
    let label_tags = meta.format_display_label(true);
    let label_files = meta.format_display_label(false);

    assert!(label_tags.contains("[WAV]"));
    assert!(label_tags.contains("(00:02)"));
    assert_eq!(label_files, "test_audio.wav");
    assert_ne!(label_tags, label_files);

    // Queue labels should NOT contain file type / format badge
    let q_tags = meta.format_queue_label(true);
    let q_files = meta.format_queue_label(false);
    assert!(!q_tags.contains("[WAV]"));
    assert!(!q_files.contains("[WAV]"));
    assert!(q_tags.contains("(00:02)"));
    assert_eq!(q_files, "test_audio.wav (00:02)");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// A fresh temp dir for one test.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("quickstream_meta_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Placeholder metadata (tags not read yet, or unreadable) knows the file's name, format and
/// size, and nothing about the audio: no made-up bitrate, sample rate, channels or length.
#[test]
fn test_fallback_metadata_leaves_audio_properties_unknown() {
    common::isolate();
    let dir = temp_dir("fallback");
    // Not audio the tag reader can parse (like FLAC-in-Ogg).
    let path = dir.join("02 - cjk.ogg");
    fs::write(&path, vec![0x55u8; 40_000]).unwrap();
    assert!(extract_metadata(&path).is_err());

    let meta = quickstream::metadata::fallback_metadata(&path);
    assert_eq!(meta.title, "02 - cjk");
    assert_eq!(meta.format_name, "OGG");
    assert_eq!(meta.file_size, 40_000);
    assert_eq!(meta.bitrate_kbps, 0, "bitrate unknown");
    assert_eq!(meta.sample_rate_hz, 0, "sample rate unknown");
    assert_eq!(meta.channels, 0, "channels unknown");
    assert_eq!(meta.duration_seconds, 0.0, "length unknown");
    let _ = fs::remove_dir_all(&dir);
}

/// The scoped scan's placeholders for files it hasn't read leave the audio properties unknown.
#[test]
fn test_scoped_scan_placeholders_leave_audio_properties_unknown() {
    common::isolate();
    let dir = temp_dir("scoped");
    fs::create_dir_all(dir.join("Band/Record")).unwrap();
    create_dummy_wav(&dir.join("Band/Record/01 Song.wav"));

    let tracks = LibraryManager::scan_directory_scoped_with_cache(&dir, &std::collections::HashMap::new());
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].format_name, "WAV");
    assert_eq!(
        (tracks[0].sample_rate_hz, tracks[0].channels),
        (0, 0),
        "{:?}",
        tracks[0]
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Autopilot candidates built from the analysis database, or from a file whose tags can't be
/// read, leave the audio properties unknown too.
#[test]
fn test_autopilot_placeholders_leave_audio_properties_unknown() {
    common::isolate();
    let dir = temp_dir("autopilot");
    fs::create_dir_all(dir.join("Band A/Record")).unwrap();
    fs::create_dir_all(dir.join("Band B/Record")).unwrap();
    let unreadable = dir.join("Band A/Record/01 Noise.mp3");
    fs::write(&unreadable, vec![0x55u8; 4_000]).unwrap();
    let analysed = dir.join("Band B/Record/01 Song.wav");
    create_dummy_wav(&analysed);
    let db = AnalysisDatabase::open_in_memory().unwrap();
    db.upsert_track(&TrackFeatures {
        filepath: analysed.clone(),
        title: "Song".to_string(),
        ..TrackFeatures::default()
    })
    .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    LibraryManager::scan_directory_autopilot_with_profile_limits(
        dir.clone(),
        Vec::new(),
        Default::default(),
        MixProfile::DiverseShuffle,
        db,
        tx,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ProfileScanLimits::default(),
    );
    let tracks: Vec<_> = rx
        .try_iter()
        .filter_map(|e| match e {
            AutopilotEvent::Track(t) => Some(t),
            AutopilotEvent::Finished(_) => None,
        })
        .collect();
    for path in [&unreadable, &analysed] {
        let track = tracks
            .iter()
            .find(|t| &t.filepath == path)
            .unwrap_or_else(|| panic!("{} not queued: {tracks:?}", path.display()));
        assert_eq!((track.sample_rate_hz, track.channels), (0, 0), "{track:?}");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// What lofty couldn't read stays 0 (unknown): no channel count is made up. A missing length is
/// estimated from the bitrate and file size, an estimate from real data the queue needs.
#[test]
fn test_audio_properties_leave_unread_channels_unknown() {
    common::isolate();
    use lofty::properties::FileProperties;
    use quickstream::metadata::audio_properties;
    use std::time::Duration;

    let unread = FileProperties::new(Duration::ZERO, None, Some(320), Some(44_100), None, None, None);
    let props = audio_properties(&unread, 4_000_000);
    assert_eq!(props.channels, 0, "no channel count is made up");
    assert_eq!(props.bitrate_kbps, 320);
    assert_eq!(props.sample_rate_hz, 44_100);
    assert!((props.duration_seconds - 100.0).abs() < 1e-9, "{}", props.duration_seconds);

    let read = FileProperties::new(Duration::from_secs(200), None, Some(900), Some(48_000), None, Some(1), None);
    let props = audio_properties(&read, 4_000_000);
    assert_eq!(props.channels, 1);
    assert_eq!(props.duration_seconds, 200.0);

    // Nothing to estimate from: unknown.
    let nothing = FileProperties::new(Duration::ZERO, None, None, None, None, None, None);
    let props = audio_properties(&nothing, 4_000_000);
    assert_eq!(
        (props.duration_seconds, props.bitrate_kbps, props.sample_rate_hz, props.channels),
        (0.0, 0, 0, 0)
    );
}
