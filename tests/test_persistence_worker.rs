//! Issue #28: bookmark and settings saves run on one persistence worker thread, never on the
//! UI thread, and queued writes are flushed on shutdown.
//!
//! Own test binary: the AppUI tests point the XDG config/cache/data dirs at a temp dir for the
//! whole process, so nothing here touches the user's real files.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use quickstream::persist::{PersistenceWorker, Writer};
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

type Log = Arc<Mutex<Vec<(PathBuf, Vec<u8>)>>>;

/// A writer that records each write, then blocks until released (or 3s pass).
fn gated_writer() -> (Writer, Log, Arc<AtomicBool>) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(AtomicBool::new(false));
    let (l, r) = (log.clone(), release.clone());
    let writer: Writer = Arc::new(move |path: &Path, bytes: &[u8]| {
        l.lock().unwrap().push((path.to_path_buf(), bytes.to_vec()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !r.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    });
    (writer, log, release)
}

fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_persist_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Point every XDG dir at a temp root for this process (once) and return the root.
fn xdg_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("qs_persist_xdg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for var in ["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"] {
            std::env::set_var(var, root.join(var.to_lowercase()));
        }
        root
    })
}

#[test]
fn test_save_returns_while_writer_is_busy_and_rapid_saves_coalesce() {
    let (writer, log, release) = gated_writer();
    let worker = PersistenceWorker::spawn_with_writer(writer);
    let path = PathBuf::from("/nonexistent/qs/config.json");

    let start = Instant::now();
    worker.save(path.clone(), b"v1".to_vec());
    assert!(wait_for(|| log.lock().unwrap().len() == 1), "worker never started the first write");
    worker.save(path.clone(), b"v2".to_vec());
    worker.save(path.clone(), b"v3".to_vec());
    worker.save(path.clone(), b"v4".to_vec());
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "save() blocked the caller for {:?} while the writer was busy",
        start.elapsed()
    );

    release.store(true, Ordering::SeqCst);
    assert!(worker.flush());
    let writes: Vec<Vec<u8>> = log.lock().unwrap().iter().map(|(_, b)| b.clone()).collect();
    assert_eq!(writes, vec![b"v1".to_vec(), b"v4".to_vec()], "queued saves of one file coalesce to the latest");
}

#[test]
fn test_drop_flushes_queued_writes() {
    let dir = scratch("drop");
    let path = dir.join("bookmarks.json");
    let worker = PersistenceWorker::spawn();
    worker.save(path.clone(), b"[\"/a\"]".to_vec());
    worker.save(path.clone(), b"[\"/a\",\"/b\"]".to_vec());
    drop(worker);
    assert_eq!(std::fs::read(&path).unwrap(), b"[\"/a\",\"/b\"]");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_handle_outliving_worker_still_writes() {
    let dir = scratch("late");
    let path = dir.join("config.json");
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    drop(worker);
    handle.save(path.clone(), b"{}".to_vec());
    assert_eq!(std::fs::read(&path).unwrap(), b"{}", "a save after shutdown falls back to a direct write");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_settings_manager_save_goes_through_worker() {
    let dir = scratch("settings");
    let path = dir.join("config.json");
    let (writer, log, release) = gated_writer();
    let worker = PersistenceWorker::spawn_with_writer(writer);
    let mut mgr = SettingsManager::with_path(path.clone());
    mgr.set_persistence(worker.handle());

    let start = Instant::now();
    mgr.settings.volume_step = 10;
    assert!(mgr.save());
    assert!(wait_for(|| log.lock().unwrap().len() == 1));
    mgr.cycle_setting(5, true); // force_transcode, saves again
    assert!(start.elapsed() < Duration::from_millis(500), "settings save blocked for {:?}", start.elapsed());

    release.store(true, Ordering::SeqCst);
    assert!(worker.flush());
    let (last_path, last_bytes) = log.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last_path, path);
    let saved: quickstream::settings::Settings = serde_json::from_slice(&last_bytes).unwrap();
    assert_eq!(saved.volume_step, 10);
    assert!(saved.force_transcode);
    assert!(!path.exists(), "the instrumented writer is the only writer");
    let _ = std::fs::remove_dir_all(&dir);
}

fn test_app(config: &Path) -> AppUI {
    let mut settings_mgr = SettingsManager::with_path(config.to_path_buf());
    settings_mgr.settings.web_server_enabled = false;
    AppUI::new(Some(std::env::temp_dir()), settings_mgr)
}

#[test]
fn test_ui_bookmark_and_setting_saves_do_not_block_the_ui_thread() {
    let root = xdg_root();
    let config = root.join("ui_gated_config.json");
    let mut app = test_app(&config);
    let (writer, log, release) = gated_writer();
    app.set_persistence_worker(PersistenceWorker::spawn_with_writer(writer));

    let start = Instant::now();
    app.active_tab = 1;
    app.handle_tab_key(KeyCode::Char('b')); // toggle bookmark
    assert!(wait_for(|| log.lock().unwrap().len() == 1));
    app.active_tab = 3;
    app.settings_cursor = 3; // volume step
    app.handle_tab_key(KeyCode::Right);
    assert!(start.elapsed() < Duration::from_millis(500), "UI blocked on a save for {:?}", start.elapsed());

    release.store(true, Ordering::SeqCst);
    assert!(app.persistence.flush());
    let log = log.lock().unwrap();
    let bookmarks_path = quickstream::paths::get_bookmarks_path();
    assert!(bookmarks_path.starts_with(root), "test must not use the real config dir");
    assert!(log.iter().any(|(p, b)| p == &bookmarks_path
        && String::from_utf8_lossy(b).contains(&*std::env::temp_dir().to_string_lossy())));
    assert!(log.iter().any(|(p, _)| p == &config));
}

#[test]
fn test_ui_change_right_before_quit_is_flushed_on_drop() {
    let root = xdg_root();
    let config = root.join("ui_flush_config.json");
    let mut app = test_app(&config);
    let before = app.settings_mgr.settings.volume_step;
    app.active_tab = 3;
    app.settings_cursor = 3;
    app.handle_tab_key(KeyCode::Right);
    let after = app.settings_mgr.settings.volume_step;
    assert_ne!(before, after);
    drop(app);

    let saved = SettingsManager::load_from(&config).expect("settings flushed on shutdown");
    assert_eq!(saved.volume_step, after);
}
