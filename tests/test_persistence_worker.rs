//! Issue #28: bookmark and settings saves run on one persistence worker thread, never on the
//! UI thread, and queued writes are flushed on shutdown.
//!
//! Like every test binary, the XDG config/cache/data dirs point at a temp dir for the whole
//! process (`common::isolate()`), so nothing here touches the user's real files.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

#[test]
fn test_save_returns_while_writer_is_busy_and_rapid_saves_coalesce() {
    common::isolate();
    let (writer, log, release) = gated_writer();
    let worker = PersistenceWorker::spawn_with_writer(writer);
    let path = PathBuf::from("/nonexistent/qs/config.json");

    let start = Instant::now();
    worker.save(path.clone(), b"v1".to_vec());
    assert!(
        wait_for(|| log.lock().unwrap().len() == 1),
        "worker never started the first write"
    );
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
    assert_eq!(
        writes,
        vec![b"v1".to_vec(), b"v4".to_vec()],
        "queued saves of one file coalesce to the latest"
    );
}

#[test]
fn test_drop_flushes_queued_writes() {
    common::isolate();
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
    common::isolate();
    let dir = scratch("late");
    let path = dir.join("config.json");
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    drop(worker);
    handle.save(path.clone(), b"{}".to_vec());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"{}",
        "a save after shutdown falls back to a direct write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_settings_manager_save_goes_through_worker() {
    common::isolate();
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
    mgr.cycle_setting(
        quickstream::settings::setting::FORCE_TRANSCODE,
        true,
        &Default::default(),
    ); // saves again
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "settings save blocked for {:?}",
        start.elapsed()
    );

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
    let root = common::isolate();
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
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "UI blocked on a save for {:?}",
        start.elapsed()
    );

    release.store(true, Ordering::SeqCst);
    assert!(app.persistence.flush());
    let log = log.lock().unwrap();
    let bookmarks_path = quickstream::paths::get_bookmarks_path();
    assert!(
        bookmarks_path.starts_with(root),
        "test must not use the real config dir"
    );
    assert!(log
        .iter()
        .any(|(p, b)| p == &bookmarks_path
            && String::from_utf8_lossy(b).contains(&*std::env::temp_dir().to_string_lossy())));
    assert!(log.iter().any(|(p, _)| p == &config));
}

#[test]
fn test_ui_change_right_before_quit_is_flushed_on_drop() {
    let root = common::isolate();
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

// ---------- deletes and renames (issue #37: playlist files) ----------

/// File names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn test_delete_removes_the_file() {
    common::isolate();
    let dir = scratch("delete");
    let path = dir.join("Road Trip.m3u8");
    std::fs::write(&path, b"#EXTM3U\n").unwrap();
    let worker = PersistenceWorker::spawn();

    worker.delete(path.clone());
    assert!(worker.flush());
    assert!(!path.exists(), "delete removes the file");

    worker.delete(path.clone()); // already gone: nothing to do, the worker keeps running
    worker.save(dir.join("after.m3u8"), b"ok".to_vec());
    assert!(worker.flush());
    assert_eq!(names_in(&dir), vec!["after.m3u8".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_rename_moves_the_file() {
    common::isolate();
    let dir = scratch("rename");
    let from = dir.join("Road Trip.m3u8");
    let to = dir.join("Holiday.m3u8");
    std::fs::write(&from, b"#EXTM3U\n/a.flac\n").unwrap();
    let worker = PersistenceWorker::spawn();

    worker.rename(from.clone(), to.clone());
    assert!(worker.flush());
    assert!(!from.exists(), "old name is gone");
    assert_eq!(std::fs::read(&to).unwrap(), b"#EXTM3U\n/a.flac\n");
    assert_eq!(names_in(&dir), vec!["Holiday.m3u8".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_then_delete_leaves_no_file() {
    common::isolate();
    let dir = scratch("save_delete");
    let path = dir.join("Road Trip.m3u8");
    let worker = PersistenceWorker::spawn();

    worker.save(path.clone(), b"v1".to_vec());
    worker.delete(path.clone());
    assert!(worker.flush());
    assert!(names_in(&dir).is_empty(), "left behind: {:?}", names_in(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_delete_then_save_leaves_the_new_bytes() {
    common::isolate();
    let dir = scratch("delete_save");
    let path = dir.join("Road Trip.m3u8");
    std::fs::write(&path, b"old").unwrap();
    let worker = PersistenceWorker::spawn();

    worker.delete(path.clone());
    worker.save(path.clone(), b"new".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_after_a_delete_is_not_merged_into_the_earlier_save() {
    common::isolate();
    let dir = scratch("save_delete_save");
    let path = dir.join("Road Trip.m3u8");
    let worker = PersistenceWorker::spawn();

    // Merging v2 into the v1 slot would write v2 and then delete it.
    worker.save(path.clone(), b"v1".to_vec());
    worker.delete(path.clone());
    worker.save(path.clone(), b"v2".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_rename_save_leaves_only_the_new_name_with_the_last_bytes() {
    common::isolate();
    let dir = scratch("save_rename_save");
    let old = dir.join("Road Trip.m3u8");
    let new = dir.join("Holiday.m3u8");
    let worker = PersistenceWorker::spawn();

    worker.save(old.clone(), b"#PLAYLIST:Road Trip\n".to_vec());
    worker.rename(old.clone(), new.clone());
    worker.save(new.clone(), b"#PLAYLIST:Holiday\n".to_vec());
    assert!(worker.flush());
    assert_eq!(names_in(&dir), vec!["Holiday.m3u8".to_string()]);
    assert_eq!(std::fs::read(&new).unwrap(), b"#PLAYLIST:Holiday\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_to_the_old_name_after_a_rename_is_not_merged_into_the_earlier_save() {
    common::isolate();
    let dir = scratch("save_rename_save_old");
    let old = dir.join("a.m3u8");
    let new = dir.join("b.m3u8");
    let worker = PersistenceWorker::spawn();

    // Merging the second save of a.m3u8 into the first would carry v2 along to b.m3u8.
    worker.save(old.clone(), b"v1".to_vec());
    worker.rename(old.clone(), new.clone());
    worker.save(old.clone(), b"v2".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&new).unwrap(), b"v1");
    assert_eq!(std::fs::read(&old).unwrap(), b"v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_case_only_rename_keeps_one_file_with_the_new_case() {
    common::isolate();
    let dir = scratch("case_rename");
    let lower = dir.join("road trip.m3u8");
    let upper = dir.join("Road Trip.m3u8");
    let worker = PersistenceWorker::spawn();

    // On a case-insensitive disk (the macOS default) these are one file: the OS rename must
    // change its case, and the save after it must not land under the old spelling.
    worker.save(lower.clone(), b"#PLAYLIST:road trip\n/a.flac\n".to_vec());
    worker.rename(lower.clone(), upper.clone());
    worker.save(upper.clone(), b"#PLAYLIST:Road Trip\n/a.flac\n".to_vec());
    assert!(worker.flush());
    assert_eq!(names_in(&dir), vec!["Road Trip.m3u8".to_string()]);
    assert_eq!(std::fs::read(&upper).unwrap(), b"#PLAYLIST:Road Trip\n/a.flac\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_many_rapid_saves_end_with_the_last_bytes() {
    common::isolate();
    let dir = scratch("rapid");
    let path = dir.join("Road Trip.m3u8");
    let other = dir.join("Holiday.m3u8");
    let worker = PersistenceWorker::spawn();

    // Holding K/J in the tracks pane: one save per keypress, interleaved with another file.
    for i in 0..200 {
        worker.save(path.clone(), format!("v{}", i).into_bytes());
        if i % 50 == 0 {
            worker.save(other.clone(), format!("o{}", i).into_bytes());
        }
    }
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"v199");
    assert_eq!(std::fs::read(&other).unwrap(), b"o150");
    assert_eq!(
        names_in(&dir),
        vec!["Holiday.m3u8".to_string(), "Road Trip.m3u8".to_string()]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_delete_and_rename_after_shutdown_run_on_the_calling_thread() {
    common::isolate();
    let dir = scratch("late_ops");
    let a = dir.join("a.m3u8");
    let b = dir.join("b.m3u8");
    let gone = dir.join("gone.m3u8");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&gone, b"x").unwrap();
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    drop(worker);

    handle.rename(a.clone(), b.clone());
    handle.delete(gone.clone());
    assert_eq!(names_in(&dir), vec!["b.m3u8".to_string()]);
    assert_eq!(std::fs::read(&b).unwrap(), b"a");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A playlist rename sends rename(old, new) then save(new). When the rename fails for a reason
/// other than the old file being gone, the save must not create the new file next to the old
/// one, or both load as playlists on the next start.
#[test]
fn test_save_after_a_failed_rename_does_not_create_the_target() {
    common::isolate();
    let dir = scratch("failed_rename");
    let locked = dir.join("locked");
    let open = dir.join("open");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::create_dir_all(&open).unwrap();
    let old = locked.join("Old.m3u8");
    let new = open.join("New.m3u8");
    std::fs::write(&old, b"#EXTM3U\n#PLAYLIST:Old\n").unwrap();
    // Read-only folder: its entries can't be removed, so the rename fails with EACCES.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(locked.join("probe"), b"").is_ok() {
        // Running as root: permissions don't apply, nothing to test.
        let _ = std::fs::remove_file(locked.join("probe"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let worker = PersistenceWorker::spawn();

    worker.rename(old.clone(), new.clone());
    worker.save(new.clone(), b"#EXTM3U\n#PLAYLIST:New\n".to_vec());
    assert!(worker.flush());

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !new.exists(),
        "the save after a failed rename created a second playlist"
    );
    assert_eq!(std::fs::read(&old).unwrap(), b"#EXTM3U\n#PLAYLIST:Old\n");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rename whose source is already gone changes nothing on disk, so the save still writes.
#[test]
fn test_save_after_a_rename_of_a_missing_file_still_writes() {
    common::isolate();
    let dir = scratch("missing_rename");
    let old = dir.join("Old.m3u8");
    let new = dir.join("New.m3u8");
    let worker = PersistenceWorker::spawn();

    worker.rename(old.clone(), new.clone());
    worker.save(new.clone(), b"#EXTM3U\n".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&new).unwrap(), b"#EXTM3U\n");
    assert_eq!(names_in(&dir), vec!["New.m3u8".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR #65 review: a failed rename leaves the writer noting that the target name stands for the
/// old file. A later rename onto that name whose source is gone moves nothing, so the note is
/// stale: saves to the name must write it, and a delete of it must never follow the note to the
/// other playlist's file.
#[test]
fn test_rename_of_a_missing_file_clears_an_earlier_failure_on_the_target() {
    common::isolate();
    let dir = scratch("stale_stranded");
    let a = dir.join("A.m3u8");
    let b = dir.join("B.m3u8");
    let c = dir.join("C.m3u8");
    std::fs::write(&a, b"#EXTM3U\n#PLAYLIST:A\n").unwrap();
    std::fs::write(&b, b"foreign").unwrap();
    let worker = PersistenceWorker::spawn();

    // A -> B fails: B.m3u8 is someone else's file.
    worker.rename(a.clone(), b.clone());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&b).unwrap(), b"foreign");
    // Outside the app the foreign file goes away, and C.m3u8 was never there.
    std::fs::remove_file(&b).unwrap();
    worker.rename(c.clone(), b.clone());
    worker.save(b.clone(), b"#EXTM3U\n#PLAYLIST:B\n".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&b).unwrap(), b"#EXTM3U\n#PLAYLIST:B\n");

    worker.delete(b.clone());
    assert!(worker.flush());
    assert_eq!(
        names_in(&dir),
        vec!["A.m3u8".to_string()],
        "the delete removed the wrong file"
    );
    assert_eq!(std::fs::read(&a).unwrap(), b"#EXTM3U\n#PLAYLIST:A\n");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- creates that never replace, and failures reported back (issues #62, #52) ----------

use quickstream::persist::{PersistFailure, PersistReport, Reporter};
use std::sync::mpsc;

/// Failures reported on `rx` so far (the acknowledgements of operations that worked are dropped).
fn reported(rx: &mpsc::Receiver<PersistReport>) -> Vec<PersistFailure> {
    rx.try_iter()
        .filter_map(|report| match report {
            PersistReport::Failed(failure) => Some(failure),
            PersistReport::Done { .. } => None,
        })
        .collect()
}

/// PR #65 review: every create and rename queued with a reporter answers exactly once, so the
/// store knows which names the writer is still working on. One that worked, a rename of a file
/// that is already gone and a rename from a path whose own rename failed each say `Done`.
#[test]
fn test_every_reported_create_and_rename_answers_once() {
    common::isolate();
    let dir = scratch("acks");
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    let path = |name: &str| dir.join(format!("{}.m3u8", name));
    std::fs::write(path("Theirs"), b"theirs").unwrap();

    handle.create(path("A"), b"a".to_vec(), Reporter::new(1, tx.clone()));
    handle.rename_reporting(path("A"), path("B"), Reporter::new(2, tx.clone()));
    handle.rename_reporting(path("Gone"), path("C"), Reporter::new(3, tx.clone()));
    handle.rename_reporting(path("B"), path("Theirs"), Reporter::new(4, tx.clone()));
    handle.rename_reporting(path("Theirs"), path("D"), Reporter::new(5, tx.clone()));
    handle.create(path("Theirs"), b"x".to_vec(), Reporter::new(6, tx));
    assert!(worker.flush());

    let reports: Vec<PersistReport> = rx.try_iter().collect();
    let tags: Vec<u64> = reports.iter().map(PersistReport::tag).collect();
    assert_eq!(tags, vec![1, 2, 3, 4, 5, 6], "{:?}", reports);
    let failed: Vec<u64> = reports
        .iter()
        .filter(|r| matches!(r, PersistReport::Failed(_)))
        .map(PersistReport::tag)
        .collect();
    assert_eq!(failed, vec![4, 6]);
    assert_eq!(std::fs::read(path("Theirs")).unwrap(), b"theirs");
    assert_eq!(std::fs::read(path("B")).unwrap(), b"a");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_create_writes_a_new_file_and_reports_nothing() {
    common::isolate();
    let dir = scratch("create_new");
    let path = dir.join("sub").join("Mix.m3u8");
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();

    worker
        .handle()
        .create(path.clone(), b"#EXTM3U\n".to_vec(), Reporter::new(1, tx));
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"#EXTM3U\n");
    assert_eq!(
        names_in(&dir.join("sub")),
        vec!["Mix.m3u8".to_string()],
        "no temp file left"
    );
    assert!(reported(&rx).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #62: the store's name check can't model every file system's name rules, so a create
/// can land on a file the store never saw. The writer must leave that file alone and say so.
#[test]
fn test_create_never_replaces_an_existing_file_and_reports_it() {
    common::isolate();
    let dir = scratch("create_exists");
    let path = dir.join("Mix.m3u8");
    std::fs::write(&path, b"#EXTM3U\n/user/own.flac\n").unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();

    worker
        .handle()
        .create(path.clone(), b"#EXTM3U\n#PLAYLIST:Mix\n".to_vec(), Reporter::new(7, tx));
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"#EXTM3U\n/user/own.flac\n");
    assert_eq!(names_in(&dir), vec!["Mix.m3u8".to_string()], "no temp file left");
    match reported(&rx).as_slice() {
        [PersistFailure::Create {
            tag: 7, path: p, kind, ..
        }] => {
            assert_eq!(p, &path);
            assert_eq!(*kind, std::io::ErrorKind::AlreadyExists);
        }
        other => panic!("expected one create failure, got {:?}", other),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// After a create collided, the playlist's later saves (queued before the store heard about it)
/// and its delete must not touch the file that was already there, in this batch or a later one.
#[test]
fn test_saves_and_deletes_after_a_failed_create_leave_the_file_alone() {
    common::isolate();
    let dir = scratch("create_exists_later");
    let path = dir.join("Mix.m3u8");
    std::fs::write(&path, b"theirs").unwrap();
    let (tx, _rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();

    handle.create(path.clone(), b"ours v1".to_vec(), Reporter::new(1, tx));
    handle.save(path.clone(), b"ours v2".to_vec());
    assert!(worker.flush());
    handle.save(path.clone(), b"ours v3".to_vec());
    handle.delete(path.clone());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"theirs");

    // Renaming the playlist moves nothing either: the file at its old name is not its own.
    let moved = dir.join("Mix 2.m3u8");
    handle.rename(path.clone(), moved.clone());
    handle.save(moved.clone(), b"ours v4".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
    assert!(!moved.exists(), "a save after the refused rename created a file");

    // The worker is gone: saves run on the calling thread and still refuse the path.
    drop(worker);
    handle.save(path.clone(), b"ours v5".to_vec());
    handle.save(moved.clone(), b"ours v5".to_vec());
    assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
    assert!(!moved.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_create_after_shutdown_still_refuses_and_reports() {
    common::isolate();
    let dir = scratch("create_late");
    let taken = dir.join("Mix.m3u8");
    let free = dir.join("Road Trip.m3u8");
    std::fs::write(&taken, b"theirs").unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    drop(worker);

    handle.create(taken.clone(), b"ours".to_vec(), Reporter::new(3, tx.clone()));
    handle.create(free.clone(), b"ours".to_vec(), Reporter::new(4, tx));
    assert_eq!(std::fs::read(&taken).unwrap(), b"theirs");
    assert_eq!(std::fs::read(&free).unwrap(), b"ours");
    let tags: Vec<u64> = reported(&rx)
        .iter()
        .map(|f| match f {
            PersistFailure::Create { tag, .. } | PersistFailure::Rename { tag, .. } => *tag,
        })
        .collect();
    assert_eq!(tags, vec![3]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A save queued after a create is not merged ahead of it: the create would then find the
/// playlist's own file and fail.
#[test]
fn test_create_then_saves_end_with_the_last_bytes() {
    common::isolate();
    let dir = scratch("create_save");
    let path = dir.join("Mix.m3u8");
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();

    worker
        .handle()
        .create(path.clone(), b"v0".to_vec(), Reporter::new(1, tx));
    worker.save(path.clone(), b"v1".to_vec());
    worker.save(path.clone(), b"v2".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"v2");
    assert!(reported(&rx).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #52: a rename that fails is reported, and saves to the new name sent in later batches
/// (before the store heard about the failure) never create the new file next to the old one.
#[test]
fn test_failed_rename_is_reported_and_later_batches_do_not_create_the_target() {
    common::isolate();
    let dir = scratch("failed_rename_report");
    let locked = dir.join("locked");
    let open = dir.join("open");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::create_dir_all(&open).unwrap();
    let old = locked.join("Old.m3u8");
    let new = open.join("New.m3u8");
    std::fs::write(&old, b"#EXTM3U\n#PLAYLIST:Old\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(locked.join("probe"), b"").is_ok() {
        // Running as root: permissions don't apply, nothing to test.
        let _ = std::fs::remove_file(locked.join("probe"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();

    worker
        .handle()
        .rename_reporting(old.clone(), new.clone(), Reporter::new(9, tx));
    worker.save(new.clone(), b"#EXTM3U\n#PLAYLIST:New\n".to_vec());
    assert!(worker.flush());
    worker.save(new.clone(), b"#EXTM3U\n#PLAYLIST:New\n/a.flac\n".to_vec());
    assert!(worker.flush());

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!new.exists(), "a later save created a second playlist");
    assert_eq!(std::fs::read(&old).unwrap(), b"#EXTM3U\n#PLAYLIST:Old\n");
    match reported(&rx).as_slice() {
        [PersistFailure::Rename {
            tag: 9, from, to, kind, ..
        }] => {
            assert_eq!((from, to), (&old, &new));
            assert_eq!(*kind, std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("expected one rename failure, got {:?}", other),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rename never replaces another file: the target is left as it was, the failure is
/// reported, and a delete of the new name removes the playlist's real file (the old name).
#[test]
fn test_rename_never_replaces_another_file() {
    common::isolate();
    let dir = scratch("rename_onto_file");
    let old = dir.join("Old.m3u8");
    let new = dir.join("New.m3u8");
    std::fs::write(&old, b"ours").unwrap();
    std::fs::write(&new, b"theirs").unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();

    worker
        .handle()
        .rename_reporting(old.clone(), new.clone(), Reporter::new(2, tx));
    worker.save(new.clone(), b"ours, renamed".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&old).unwrap(), b"ours");
    assert_eq!(std::fs::read(&new).unwrap(), b"theirs");
    match reported(&rx).as_slice() {
        [PersistFailure::Rename { tag: 2, kind, .. }] => assert_eq!(*kind, std::io::ErrorKind::AlreadyExists),
        other => panic!("expected one rename failure, got {:?}", other),
    }

    worker.delete(new.clone());
    assert!(worker.flush());
    assert!(!old.exists(), "deleting the playlist removes its real file");
    assert_eq!(std::fs::read(&new).unwrap(), b"theirs");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR #65 review (CC2): after a rename A -> B failed, renaming B back to A lands on the file that
/// really is A, so A is ours again: later saves of A write it instead of being dropped.
#[test]
fn test_renaming_back_onto_the_real_file_after_a_failed_rename_keeps_saving() {
    common::isolate();
    let dir = scratch("rename_back");
    let a = dir.join("A.m3u8");
    let b = dir.join("B.m3u8");
    std::fs::write(&a, b"ours").unwrap();
    std::fs::write(&b, b"theirs").unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();

    worker
        .handle()
        .rename_reporting(a.clone(), b.clone(), Reporter::new(1, tx));
    worker.rename(b.clone(), a.clone());
    worker.save(a.clone(), b"ours v2".to_vec());
    assert!(worker.flush());
    worker.save(a.clone(), b"ours v3".to_vec());
    assert!(worker.flush());

    assert_eq!(std::fs::read(&a).unwrap(), b"ours v3");
    assert_eq!(std::fs::read(&b).unwrap(), b"theirs");
    assert!(matches!(
        reported(&rx).as_slice(),
        [PersistFailure::Rename { tag: 1, .. }]
    ));

    // And deleting A removes A, not the file that was in the way.
    worker.delete(a.clone());
    assert!(worker.flush());
    assert!(!a.exists());
    assert_eq!(std::fs::read(&b).unwrap(), b"theirs");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_failed_rename_after_shutdown_is_reported() {
    common::isolate();
    let dir = scratch("rename_late_report");
    let old = dir.join("Old.m3u8");
    let new = dir.join("New.m3u8");
    std::fs::write(&old, b"ours").unwrap();
    std::fs::write(&new, b"theirs").unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    drop(worker);

    handle.rename_reporting(old.clone(), new.clone(), Reporter::new(5, tx));
    handle.save(new.clone(), b"ours, renamed".to_vec());
    assert_eq!(std::fs::read(&old).unwrap(), b"ours");
    assert_eq!(std::fs::read(&new).unwrap(), b"theirs");
    assert!(matches!(
        reported(&rx).as_slice(),
        [PersistFailure::Rename { tag: 5, .. }]
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
