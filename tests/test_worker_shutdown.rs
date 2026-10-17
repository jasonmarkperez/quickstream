//! Background worker threads are tracked, stopped and joined on shutdown (issue #5).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

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

fn library(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("qs_worker_shutdown_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for artist in 0..4 {
        let album = root.join(format!("Artist {}", artist)).join("Album");
        std::fs::create_dir_all(&album).unwrap();
        for track in 0..3 {
            write_silent_wav(&album.join(format!("{:02} Song.wav", track)));
        }
    }
    write_silent_wav(&root.join("loose.wav"));
    root
}

fn app_at(dir: &std::path::Path) -> AppUI {
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    AppUI::new(Some(dir.to_path_buf()), settings_mgr)
}

#[test]
fn test_shutdown_stops_and_joins_every_worker_kind() {
    let root = library("all");
    let mut app = app_at(&root);

    app.refresh_browser(); // directory listing thread
    app.start_background_metadata_fetch(vec![root.join("loose.wav")]); // browser tag reader
    app.queue.add_track(quickstream::metadata::fallback_metadata(&root.join("loose.wav")));
    app.start_queue_metadata_fetch(); // persistent 4-thread queue metadata pool
    app.open_fuzzy_search(); // search scanner
    app.start_autopilot(root.clone()); // autopilot scanner
    assert!(app.live_worker_count() > 0);

    let start = Instant::now();
    let still_running = app.shutdown_workers(Duration::from_secs(2));
    assert_eq!(still_running, 0, "every worker should exit once signalled");
    assert_eq!(app.live_worker_count(), 0);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn test_shutdown_wait_is_bounded_for_stuck_workers() {
    let root = library("stuck");
    let mut app = app_at(&root);
    app.workers.spawn("stuck-test", || std::thread::sleep(Duration::from_secs(10)));

    let start = Instant::now();
    let still_running = app.shutdown_workers(Duration::from_millis(200));
    assert_eq!(still_running, 1);
    assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
    assert_eq!(app.live_worker_count(), 0, "unfinished workers are detached, not kept");
}

#[test]
fn test_finished_workers_are_reaped() {
    let root = library("reap");
    let mut app = app_at(&root);
    let _ = app.shutdown_workers(Duration::from_secs(2));
    app.workers.spawn("quick-test", || {});
    let deadline = Instant::now() + Duration::from_secs(2);
    while app.live_worker_count() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(app.live_worker_count(), 0);
}
