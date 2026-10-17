//! The UI's metadata cache is shared with background scanners, never deep-copied (issue #4).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use quickstream::library::{AutopilotEvent, LibraryManager, MetadataCache};
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_meta_cache_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_silent_wav(path: &std::path::Path) {
    let samples: u32 = 8_000;
    let data_len = samples * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8_000u32.to_le_bytes());
    bytes.extend_from_slice(&16_000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(bytes.len() + data_len as usize, 0);
    std::fs::write(path, bytes).unwrap();
}

fn meta(path: &std::path::Path, title: &str) -> AudioMetadata {
    AudioMetadata {
        filepath: path.to_path_buf(),
        filename: path.file_name().unwrap().to_string_lossy().to_string(),
        title: title.to_string(),
        artist: "Cached Artist".to_string(),
        file_size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        ..Default::default()
    }
}

#[test]
fn test_metadata_cache_clones_share_storage() {
    let a = MetadataCache::default();
    let b = a.clone();
    let path = PathBuf::from("/qs-test-nonexistent/a.mp3");
    a.insert(meta(&path, "Shared"));
    assert_eq!(b.get(&path).unwrap().title, "Shared");
    assert!(b.contains(&path));
    assert_eq!(b.len(), 1);
    assert!(a.shares_storage_with(&b));
}

#[test]
fn test_browser_refresh_and_autopilot_share_the_cache() {
    let dir = temp_dir("app");
    write_silent_wav(&dir.join("song.wav"));
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(dir.clone()), settings_mgr);

    let handle = app.metadata_cache.clone();
    app.refresh_browser();
    app.start_autopilot(dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert!(handle.shares_storage_with(&app.metadata_cache));

    // Results from background extraction land in the same shared map.
    let path = PathBuf::from("/qs-test-nonexistent/late.mp3");
    app.metadata_tx.send(meta(&path, "Late Arrival")).unwrap();
    app.poll_background_metadata();
    assert_eq!(handle.get(&path).unwrap().title, "Late Arrival");
}

#[test]
fn test_autopilot_scanner_reads_shared_cache() {
    let dir = temp_dir("scanner");
    let album = dir.join("Artist").join("Album");
    std::fs::create_dir_all(&album).unwrap();
    let song = album.join("01 Song.wav");
    write_silent_wav(&song);

    let cache = MetadataCache::default();
    cache.insert(meta(&song, "From Shared Cache"));

    let (tx, rx) = mpsc::channel();
    LibraryManager::scan_directory_autopilot_with_profile(
        dir.clone(),
        Vec::new(),
        cache.clone(),
        quickstream::analysis::MixProfile::DiverseShuffle,
        quickstream::analysis::AnalysisDatabase::open_in_memory().unwrap(),
        tx,
        Arc::new(AtomicBool::new(false)),
    );
    let titles: Vec<String> = rx
        .try_iter()
        .filter_map(|e| match e {
            AutopilotEvent::Track(t) => Some(t.title),
            AutopilotEvent::Finished(_) => None,
        })
        .collect();
    assert_eq!(titles, vec!["From Shared Cache".to_string()]);
}
