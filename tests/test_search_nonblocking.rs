//! Opening fuzzy search must not read the library cache on the UI thread (issue #3).
//!
//! The cache file is replaced with a named pipe that only produces its content after
//! `SLOW_CACHE_DELAY`, like a cache on a slow disk. Kept in its own test binary because it
//! points XDG_CACHE_HOME at a temp dir for the whole process.

mod common;

use std::io::Write;
use std::time::{Duration, Instant};

use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

const SLOW_CACHE_DELAY: Duration = Duration::from_millis(1500);

#[test]
fn test_open_fuzzy_search_loads_cache_off_ui_thread() {
    common::isolate();
    let root = std::env::temp_dir().join(format!("qs_search_nb_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let music_dir = root.join("music");
    let cache_home = root.join("cache");
    std::fs::create_dir_all(&music_dir).unwrap();
    std::fs::create_dir_all(cache_home.join("quickstream")).unwrap();

    let song = music_dir.join("song.mp3");
    std::fs::write(&song, vec![0u8; 4096]).unwrap();
    let mut cached = quickstream::metadata::fallback_metadata(&song);
    cached.artist = "Cached Artist".to_string();
    let line = format!("{}\n", serde_json::to_string(&cached).unwrap());

    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(music_dir.clone()), settings_mgr);

    std::env::set_var("XDG_CACHE_HOME", &cache_home);
    let index = cache_home.join("quickstream").join("music_index.jsonl");
    assert!(std::process::Command::new("mkfifo").arg(&index).status().unwrap().success());
    std::thread::spawn(move || {
        std::thread::sleep(SLOW_CACHE_DELAY);
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&index) {
            let _ = f.write_all(line.as_bytes());
        }
    });

    let start = Instant::now();
    app.open_fuzzy_search();
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(300), "UI thread blocked for {:?}", elapsed);

    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let modal = app.search_modal.as_mut().expect("search modal open");
        modal.poll_background_scan();
        if !modal.all_tracks.is_empty() || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let modal = app.search_modal.as_ref().unwrap();
    assert_eq!(modal.all_tracks.len(), 1);
    assert_eq!(modal.all_tracks[0].artist, "Cached Artist");
}
