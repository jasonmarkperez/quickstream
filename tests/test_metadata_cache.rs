//! The UI's metadata cache is shared with background scanners, never deep-copied (issue #4).

mod common;

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
    common::isolate();
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
    common::isolate();
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
    app.metadata_tx.send(meta(&path, "Late Arrival").into()).unwrap();
    app.poll_background_metadata();
    assert_eq!(handle.get(&path).unwrap().title, "Late Arrival");
}

#[test]
fn test_autopilot_scanner_reads_shared_cache() {
    common::isolate();
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
            AutopilotEvent::Candidates(_) | AutopilotEvent::Finished(_) => None,
        })
        .collect();
    assert_eq!(titles, vec!["From Shared Cache".to_string()]);
}

#[test]
fn revision_changes_on_every_insert_and_is_shared_by_clones() {
    common::isolate();
    let cache = MetadataCache::default();
    let clone = cache.clone();
    let before = cache.revision();
    clone.insert(AudioMetadata {
        filepath: PathBuf::from("/tmp/a.flac"),
        ..Default::default()
    });
    assert_ne!(
        cache.revision(),
        before,
        "an insert through a clone bumps the shared revision"
    );
    let after_one = cache.revision();
    let _ = cache.get(&PathBuf::from("/tmp/a.flac"));
    let _ = cache.contains(&PathBuf::from("/tmp/a.flac"));
    assert_eq!(cache.revision(), after_one, "reads leave it alone");
}

/// `folder_revision(dir)` moves only for inserts of files directly inside `dir`, so the UI can
/// keep a folder's totals while tags arrive for other folders (PR #68 review).
#[test]
fn folder_revision_moves_only_for_files_directly_in_the_folder() {
    common::isolate();
    let cache = MetadataCache::default();
    let clone = cache.clone();
    let dir = PathBuf::from("/music/Mira Lenz");
    let meta = |path: &str| AudioMetadata {
        filepath: PathBuf::from(path),
        ..Default::default()
    };
    let start = cache.folder_revision(&dir);

    clone.insert(meta("/music/Other/a.flac"));
    clone.insert(meta("/music/Mira Lenz/Sub/b.flac"));
    clone.insert(meta("/music/c.flac"));
    assert_eq!(
        cache.folder_revision(&dir),
        start,
        "other folders, a subfolder, the parent"
    );

    clone.insert(meta("/music/Mira Lenz/d.flac"));
    let after = cache.folder_revision(&dir);
    assert_ne!(after, start, "a file in the folder, through a clone");
    clone.insert(meta("/music/Mira Lenz/d.flac"));
    assert_ne!(cache.folder_revision(&dir), after, "replacing an entry");
    assert_ne!(
        cache.folder_revision(&PathBuf::from("/music/Mira Lenz/Sub")),
        cache.folder_revision(&PathBuf::from("/music/Never Seen")),
        "the subfolder has its own revision"
    );
}

/// The size a failed tag read found is kept apart from the entries (issue #124): no consumer of
/// the cache mistakes it for tags, a clone sees it, and tags read later replace it.
#[test]
fn unreadable_sizes_are_not_entries_and_tags_replace_them() {
    common::isolate();
    let cache = MetadataCache::default();
    let clone = cache.clone();
    let path = PathBuf::from("/qs-test-nonexistent/garbage.ogg");
    let (revision, folder) = (cache.revision(), cache.folder_revision(path.parent().unwrap()));

    clone.insert_unreadable(&path, 4_096);
    assert_eq!(cache.unreadable_size(&path), Some(4_096));
    assert!(!cache.contains(&path) && cache.get(&path).is_none() && cache.is_empty());
    assert_eq!(cache.revision(), revision, "no tags arrived");
    assert_eq!(cache.folder_revision(path.parent().unwrap()), folder);

    clone.insert_unreadable(&path, 8_192);
    assert_eq!(
        cache.unreadable_size(&path),
        Some(8_192),
        "a later failed read replaces it"
    );

    clone.insert(meta(&path, "Read At Last"));
    assert_eq!(cache.unreadable_size(&path), None, "tags replace it");
    assert_eq!(cache.get(&path).unwrap().title, "Read At Last");
}
