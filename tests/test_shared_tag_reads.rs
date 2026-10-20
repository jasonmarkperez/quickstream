//! Issue #52: the browser's metadata fetch, the queue's metadata pool and playlist folder adds
//! share one set of paths being read, so no two of them read the same file at once, and no
//! request loses its tags because another worker read the file for it.
//!
//! A gated tag reader stands in for the share: it counts reads per path and holds each read
//! until the test opens the gate. The audio files are empty; only their names matter.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::PlaylistModal;
use quickstream::ui::tag_reads::TagReader;
use quickstream::ui::AppUI;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quickstream_tag_reads_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// App browsing `dir` with no tag reads started by the browser itself. The tests share one
/// playlists folder, so each uses a playlist name of its own.
fn new_app(dir: &Path) -> AppUI {
    let mut settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(dir.to_path_buf()), settings_mgr);
    wait_until(&mut app, "playlists to load", |a| a.playlists.store.is_loaded());
    app
}

/// Poll every channel the tag readers report on until `done` holds (about 3 s at most).
fn wait_until(app: &mut AppUI, what: &str, done: impl Fn(&AppUI) -> bool) {
    for _ in 0..300 {
        app.poll_playlists();
        app.poll_background_metadata();
        app.poll_background_dir_scan();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", what);
}

/// Keep polling for `ms` so any worker that would read a file has had the chance.
fn settle(app: &mut AppUI, ms: u64) {
    for _ in 0..ms / 10 {
        app.poll_playlists();
        app.poll_background_metadata();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A gate the reader waits on until the test opens it.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        let (open, changed) = &*self.0;
        *open.lock().unwrap() = true;
        changed.notify_all();
    }

    fn wait(&self) {
        let (open, changed) = &*self.0;
        let mut guard = open.lock().unwrap();
        while !*guard {
            guard = changed.wait(guard).unwrap();
        }
    }
}

/// Every path read, in order, by any worker.
#[derive(Clone, Default)]
struct Reads(Arc<Mutex<Vec<PathBuf>>>);

impl Reads {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn of(&self, path: &Path) -> usize {
        self.0.lock().unwrap().iter().filter(|p| p.as_path() == path).count()
    }
}

/// Tags the fake reader returns: a 2-second track with an artist and title.
fn tagged(path: &Path) -> AudioMetadata {
    AudioMetadata {
        filepath: path.to_path_buf(),
        filename: path.file_name().unwrap().to_string_lossy().into_owned(),
        title: "Tagged".to_string(),
        artist: "Band".to_string(),
        album: "Record".to_string(),
        duration_seconds: 2.0,
        sample_rate_hz: 44_100,
        ..Default::default()
    }
}

/// A reader that records each read, then holds it until `gate` opens.
fn gated_reader(reads: &Reads, gate: &Gate) -> TagReader {
    let (reads, gate) = (reads.clone(), gate.clone());
    Arc::new(move |path: &Path| {
        reads.0.lock().unwrap().push(path.to_path_buf());
        gate.wait();
        Ok(tagged(path))
    })
}

fn press(app: &mut AppUI, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap();
}

/// `L` on the browser row for `folder`, then Enter on `playlist` in the add picker.
fn add_folder_to(app: &mut AppUI, folder: &Path, playlist: &str) {
    wait_until(app, "browser listing", |a| {
        !a.browser_loading && a.opening_folder().is_none()
    });
    app.browser_cursor = app
        .browser_entries
        .iter()
        .position(|(_, p, _)| p == folder)
        .expect("folder is listed in the browser");
    press(app, KeyCode::Char('L'));
    let row = app.playlists.store.index_of(playlist).expect("playlist exists");
    match app.playlists.modal.as_mut() {
        Some(PlaylistModal::AddPicker { cursor, .. }) => *cursor = row,
        _ => panic!("L on a folder should open the add picker"),
    }
    press(app, KeyCode::Enter);
}

/// An album folder with one (empty) track in it.
fn album_with_track(dir: &Path) -> (PathBuf, PathBuf) {
    let album = dir.join("Album");
    fs::create_dir_all(&album).unwrap();
    let track = album.join("01 Intro.flac");
    fs::write(&track, b"").unwrap();
    (album, track)
}

#[test]
fn test_queue_and_browser_reads_skip_a_file_a_folder_add_is_reading() {
    common::isolate();
    let dir = scratch_dir("skip");
    let (album, track) = album_with_track(&dir);
    let mut app = new_app(&dir);
    app.playlists.store.create("Skip Mix").unwrap();
    let (reads, gate) = (Reads::default(), Gate::default());
    app.set_tag_reader(gated_reader(&reads, &gate));

    add_folder_to(&mut app, &album, "Skip Mix");
    wait_until(&mut app, "the folder add to start reading", |_| reads.count() == 1);
    assert_eq!(app.tag_reads_in_flight(), 1);

    // The same file is queued as a placeholder and shown in the browser while it is read.
    app.queue.add_track(quickstream::metadata::fallback_metadata(&track));
    app.start_queue_metadata_fetch();
    app.start_background_metadata_fetch(vec![track.clone()]);
    settle(&mut app, 200);
    assert_eq!(
        reads.of(&track),
        1,
        "a second worker read the file while it was being read"
    );

    gate.open();
    wait_until(&mut app, "the folder add and the queue row", |a| {
        a.playlists.store.get("Skip Mix").is_some_and(|p| p.entries.len() == 1) && a.queue.tracks()[0].title == "Tagged"
    });
    // The queue row got the folder add's tags without a read of its own.
    assert_eq!(app.queue.tracks()[0].duration_seconds, 2.0);
    assert_eq!(reads.of(&track), 1);
    assert_eq!(app.tag_reads_in_flight(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_folder_add_waits_for_a_queue_read_and_uses_its_tags() {
    common::isolate();
    let dir = scratch_dir("wait");
    let (album, track) = album_with_track(&dir);
    let mut app = new_app(&dir);
    app.playlists.store.create("Wait Mix").unwrap();
    let (reads, gate) = (Reads::default(), Gate::default());
    app.set_tag_reader(gated_reader(&reads, &gate));

    app.queue.add_track(quickstream::metadata::fallback_metadata(&track));
    app.start_queue_metadata_fetch();
    wait_until(&mut app, "the queue read to start", |_| reads.count() == 1);

    add_folder_to(&mut app, &album, "Wait Mix");
    settle(&mut app, 200);
    assert_eq!(
        reads.of(&track),
        1,
        "the folder add read the file the queue was reading"
    );
    assert!(
        app.playlists.store.get("Wait Mix").unwrap().entries.is_empty(),
        "the folder add must wait for the tags, not add a placeholder"
    );

    gate.open();
    wait_until(&mut app, "the folder add", |a| {
        a.playlists.store.get("Wait Mix").is_some_and(|p| p.entries.len() == 1)
    });
    let entry = app.playlists.store.get("Wait Mix").unwrap().entries[0].clone();
    assert_eq!((entry.title.as_str(), entry.duration_secs), ("Tagged", 2));
    assert_eq!(reads.of(&track), 1);
    assert_eq!(app.status_message, "Added \"Tagged\" to Wait Mix (1 track)");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_failed_and_panicking_reads_release_their_paths() {
    common::isolate();
    let dir = scratch_dir("release");
    let album = dir.join("Album");
    fs::create_dir_all(&album).unwrap();
    let fails = album.join("01 Fails.flac");
    let panics = album.join("02 Panics.flac");
    fs::write(&fails, b"").unwrap();
    fs::write(&panics, b"").unwrap();
    let mut app = new_app(&dir);
    app.playlists.store.create("Release Mix").unwrap();

    // The first read of each file fails (one with an error, one by panicking); later reads work.
    let reads = Reads::default();
    let reader_reads = reads.clone();
    app.set_tag_reader(Arc::new(move |path: &Path| {
        let first = {
            let mut all = reader_reads.0.lock().unwrap();
            all.push(path.to_path_buf());
            all.iter().filter(|p| p.as_path() == path).count() == 1
        };
        if first && path.ends_with("02 Panics.flac") {
            panic!("tag reader panic (expected in test)");
        }
        if first {
            anyhow::bail!("unreadable (expected in test)");
        }
        Ok(tagged(path))
    }));

    for path in [&fails, &panics] {
        app.queue.add_track(quickstream::metadata::fallback_metadata(path));
    }
    app.start_queue_metadata_fetch();
    wait_until(&mut app, "both queue reads", |_| reads.count() == 2);
    wait_until(&mut app, "both paths released", |a| a.tag_reads_in_flight() == 0);

    // Nothing is left marked as being read, so the folder add reads both files itself.
    add_folder_to(&mut app, &album, "Release Mix");
    wait_until(&mut app, "the folder add", |a| {
        a.playlists
            .store
            .get("Release Mix")
            .is_some_and(|p| p.entries.len() == 2)
    });
    assert_eq!((reads.of(&fails), reads.of(&panics)), (2, 2));
    let durations: Vec<i64> = app
        .playlists
        .store
        .get("Release Mix")
        .unwrap()
        .entries
        .iter()
        .map(|e| e.duration_secs)
        .collect();
    assert_eq!(durations, vec![2, 2]);
    let _ = fs::remove_dir_all(&dir);
}
