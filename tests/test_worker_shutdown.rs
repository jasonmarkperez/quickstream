//! Background worker threads are tracked, stopped and joined on shutdown (issue #5).

mod common;

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
    common::isolate();
    let root = library("all");
    let mut app = app_at(&root);

    app.refresh_browser(); // directory listing thread
    app.start_background_metadata_fetch(vec![root.join("loose.wav")]); // browser tag reader
    app.queue
        .add_track(quickstream::metadata::fallback_metadata(&root.join("loose.wav")));
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
    common::isolate();
    let root = library("stuck");
    let mut app = app_at(&root);
    app.workers
        .spawn("stuck-test", || std::thread::sleep(Duration::from_secs(10)));

    let start = Instant::now();
    let still_running = app.shutdown_workers(Duration::from_millis(200));
    assert_eq!(still_running, 1);
    assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
    assert_eq!(app.live_worker_count(), 0, "unfinished workers are detached, not kept");
}

#[test]
fn test_finished_workers_are_reaped() {
    common::isolate();
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

// ---------- the library indexer (#41) ----------

/// An indexer source on its own database whose tag reads block until `release` is dropped (a
/// hung share); `entered` hears of every read.
fn hung_index_source(
    root: &std::path::Path,
) -> (
    quickstream::ui::library_index::LibraryIndexSource,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    use quickstream::analysis::index::{IndexConfig, IndexTagReader};
    use std::sync::{mpsc, Arc, Mutex};

    let db_path = root.with_extension("db");
    let _ = std::fs::remove_file(&db_path);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
    let reader: IndexTagReader = Arc::new(move |path: &std::path::Path, stamp| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(30));
        quickstream::analysis::worker::tag_features(path, stamp)
    });
    let source = quickstream::ui::library_index::LibraryIndexSource {
        open_db: Arc::new(move || quickstream::analysis::AnalysisDatabase::open_at(&db_path)),
        reader,
        config: IndexConfig {
            start_delay: Duration::ZERO,
            ..IndexConfig::default()
        },
    };
    (source, entered_rx, release_tx)
}

/// Constructing the app starts no indexer: `run()` does.
#[test]
fn test_new_starts_no_library_indexer() {
    common::isolate();
    let root = library("index_new");
    let mut app = app_at(&root);
    app.bookmarks = vec![root.to_string_lossy().into_owned()];
    app.poll_library_index();
    assert_eq!(app.library_index_starts(), 0);
    assert!(!app.library_index_running());
}

/// An indexer stuck in a tag read on a hung share is detached at the deadline; quitting waits no
/// longer for it.
#[test]
fn test_a_stuck_library_indexer_is_detached_within_the_deadline() {
    common::isolate();
    let root = library("index_stuck");
    let mut app = app_at(&root);
    app.bookmarks = vec![root.to_string_lossy().into_owned()];
    let (source, entered, release) = hung_index_source(&root);
    app.set_library_index_source(source);
    app.start_library_index();
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the indexer reads");

    let start = Instant::now();
    let still_running = app.shutdown_workers(Duration::from_millis(200));
    assert_eq!(still_running, 1, "only the stuck indexer is left");
    assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
    drop(release);
}

/// Turning the setting off and on while the old thread is still stuck starts no second thread;
/// the new one starts once the old one has finished.
#[test]
fn test_a_toggle_while_the_old_indexer_runs_starts_no_second_thread() {
    common::isolate();
    let root = library("index_toggle");
    let mut app = app_at(&root);
    app.bookmarks = vec![root.to_string_lossy().into_owned()];
    let (source, entered, release) = hung_index_source(&root);
    app.set_library_index_source(source);
    app.start_library_index();
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the indexer reads");
    assert_eq!(app.library_index_starts(), 1);

    app.settings_cursor = quickstream::settings::setting::LIBRARY_INDEX;
    app.cycle_and_apply_setting(true);
    assert!(!app.settings_mgr.settings.library_index);
    assert!(!app.library_index_running());
    app.cycle_and_apply_setting(true);
    assert!(app.settings_mgr.settings.library_index);
    for _ in 0..5 {
        app.poll_library_index();
    }
    assert_eq!(app.library_index_starts(), 1, "the stuck thread is waited for");
    app.start_library_index();
    assert_eq!(app.library_index_starts(), 1);

    // Once it has finished, the next poll starts the new one.
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.library_index_starts() < 2 && Instant::now() < deadline {
        app.poll_library_index();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(app.library_index_starts(), 2);
    assert!(app.library_index_running());
    assert_eq!(app.shutdown_workers(Duration::from_secs(2)), 0);
}
