//! Adding files to the queue never stats them on the UI thread (issue #124).
//!
//! `a` (add all), `Enter` on a file row and the web `add_file` command queue a file not in the
//! metadata cache with a placeholder built from its path alone: its size stays 0 (unknown, left
//! out of Now Playing's SIGNAL panel) until a tag read of the file publishes it. A file whose
//! size shows right after the key was looked up on the UI thread.
//!
//! A gated tag reader stands in for a slow share: the workers wait on it until the test opens
//! the gate, so nothing a worker finds can reach the queue before then.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::WebCommand;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_add_no_stat_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A mono 16-bit WAV of `samples` silent samples, with no tags.
fn write_silent_wav(path: &Path, samples: u32) {
    let data_len = samples * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&44_100u32.to_le_bytes());
    bytes.extend_from_slice(&88_200u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(bytes.len() + data_len as usize, 0);
    fs::write(path, bytes).unwrap();
}

/// A gate the tag reader waits on until the test opens it, and a count of the reads it let
/// through. Dropping the last handle does not open it: each test holds an [`OpenOnDrop`].
#[derive(Clone, Default)]
struct Gate {
    open: Arc<(Mutex<bool>, Condvar)>,
    reads: Arc<AtomicUsize>,
}

impl Gate {
    fn open(&self) {
        let (open, changed) = &*self.open;
        *open.lock().unwrap() = true;
        changed.notify_all();
    }

    fn close(&self) {
        *self.open.0.lock().unwrap() = false;
    }

    fn wait(&self) {
        let (open, changed) = &*self.open;
        let mut guard = open.lock().unwrap();
        while !*guard {
            guard = changed.wait(guard).unwrap();
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

/// Opens the gate when dropped, so a test that fails before opening it doesn't leave the workers
/// waiting on it (and the app's shutdown waiting on them). Declared after the app, so it is
/// dropped first.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// App browsing `dir` on tab [2] with the browser's own tag reads off (Filenames mode) and the
/// queue's tag reads held by `gate`, then done by the real reader.
fn gated_app(dir: &Path, gate: &Gate) -> AppUI {
    gated_app_in_mode(dir, gate, false)
}

/// [`gated_app`], in Tags mode when `tags` is set: the browser then reads the folder's tags
/// too, through the same gate.
fn gated_app_in_mode(dir: &Path, gate: &Gate, tags: bool) -> AppUI {
    let mut settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    settings_mgr.settings.metadata_view = tags;
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(dir.to_path_buf()), settings_mgr);
    let gate = gate.clone();
    app.set_tag_reader(Arc::new(move |path: &Path| {
        gate.wait();
        gate.reads.fetch_add(1, Ordering::SeqCst);
        quickstream::metadata::extract_metadata(path)
    }));
    common::wait_for_dir_scan(&mut app);
    app.active_tab = 1;
    app.queue_focus = false;
    app
}

fn press(app: &mut AppUI, code: KeyCode) {
    app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
}

/// Poll the workers' tag reads until `done` holds (5 s at most).
fn wait_until(app: &mut AppUI, what: &str, done: impl Fn(&AppUI) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.poll_background_metadata();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", what);
}

fn size_on_disk(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

#[test]
fn test_add_all_queues_untagged_files_without_stat_and_workers_fill_in_sizes() {
    common::isolate();
    let dir = scratch_dir("add_all");
    let files: Vec<PathBuf> = (0..40)
        .map(|i| {
            let path = dir.join(format!("track {:02}.wav", i));
            write_silent_wav(&path, 1_000 + 100 * i);
            path
        })
        .collect();
    let gate = Gate::default();
    let mut app = gated_app(&dir, &gate);

    let _open = OpenOnDrop(gate.clone());

    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.queue.len(), files.len());
    for track in app.queue.tracks() {
        assert!(files.contains(&track.filepath), "{:?}", track.filepath);
        assert_eq!(
            track.file_size, 0,
            "{:?} was looked up on the UI thread",
            track.filepath
        );
        assert_eq!(track.format_name, "WAV");
        assert_eq!(track.title, track.filepath.file_stem().unwrap().to_string_lossy());
    }

    gate.open();
    wait_until(&mut app, "the queue's tag reads", |a| {
        a.queue.tracks().iter().all(|t| t.file_size > 0)
    });
    for track in app.queue.tracks() {
        assert_eq!(track.file_size, size_on_disk(&track.filepath), "{:?}", track.filepath);
        assert!(track.duration_seconds > 0.0, "{:?}", track.filepath);
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A file the tag reader can't parse (FLAC-in-Ogg, say) may still play: the queue's worker
/// publishes its size alone, so Now Playing shows it once known, and leaves the row out until then.
#[test]
fn test_add_all_unreadable_files_get_their_size_from_the_queue_workers() {
    common::isolate();
    let dir = scratch_dir("unreadable");
    let current = dir.join("02 - cjk.ogg");
    let next = dir.join("03 - other.ogg");
    fs::write(&current, vec![0x55u8; 1_500_000]).unwrap();
    fs::write(&next, vec![0x55u8; 50_000]).unwrap();
    assert!(quickstream::metadata::extract_metadata(&current).is_err());
    let gate = Gate::default();
    let mut app = gated_app(&dir, &gate);
    let _open = OpenOnDrop(gate.clone());

    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.queue.len(), 2);
    assert!(app.queue.tracks().iter().all(|t| t.file_size == 0));
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(4.0, 8.0));
    app.active_tab = 0;
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("02 - CJK"), "{screen}");
    assert!(!screen.contains("SIZE"), "no size while it is unknown:\n{screen}");

    gate.open();
    wait_until(&mut app, "the sizes", |a| {
        a.queue.tracks().iter().all(|t| t.file_size > 0)
    });
    assert_eq!(app.queue.tracks()[0].file_size, 1_500_000);
    assert_eq!(app.queue.tracks()[1].file_size, 50_000);
    // Only the size: the placeholder's name and unknown audio properties stay.
    let first = &app.queue.tracks()[0];
    assert_eq!(first.title, "02 - cjk");
    assert_eq!(first.duration_seconds, 0.0);
    assert_eq!(first.sample_rate_hz, 0);
    let screen = common::tui::render_text(&mut app, 120, 40);
    let size_row = screen
        .lines()
        .find(|l| l.contains("SIZE"))
        .unwrap_or_else(|| panic!("SIZE once known:\n{screen}"));
    assert!(size_row.contains("1.4 MB"), "{size_row:?}");
    let _ = fs::remove_dir_all(&dir);
}

/// Tags mode: the browser's own tag fetch is reading the file (held at the gate) when `a` queues
/// it, so the queue's worker skips the read. The browser's read publishes the size it found when
/// the tags can't be read, so the queued row gets it from that one read.
#[test]
fn test_add_all_while_the_browser_reads_an_unreadable_file_still_gets_its_size() {
    common::isolate();
    let dir = scratch_dir("browser_busy");
    let file = dir.join("garbage.ogg");
    fs::write(&file, vec![0x55u8; 120_000]).unwrap();
    assert!(quickstream::metadata::extract_metadata(&file).is_err());
    let gate = Gate::default();
    let mut app = gated_app_in_mode(&dir, &gate, true);
    let _open = OpenOnDrop(gate.clone());
    wait_until(&mut app, "the browser's read", |a| a.tag_reads_in_flight() == 1);

    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.tracks()[0].file_size, 0, "looked up on the UI thread");

    gate.open();
    wait_until(&mut app, "the size from the browser's read", |a| {
        a.queue.tracks()[0].file_size > 0
    });
    assert_eq!(app.queue.tracks()[0].file_size, 120_000);
    assert_eq!(
        gate.reads(),
        1,
        "the queue's worker skipped the file the browser was reading"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The browser's fetch (Tags mode) failed to read a file's tags before it was queued: its read
/// published the size, so `a` queues the file with it at once, from memory, while the queue's
/// own read of the file is still held at the gate.
#[test]
fn test_size_from_a_failed_browser_read_is_used_when_the_file_is_queued() {
    common::isolate();
    let dir = scratch_dir("browser_failed");
    let file = dir.join("garbage.ogg");
    fs::write(&file, vec![0x55u8; 90_000]).unwrap();
    let gate = Gate::default();
    let mut app = gated_app_in_mode(&dir, &gate, true);
    let _open = OpenOnDrop(gate.clone());
    gate.open();
    wait_until(&mut app, "the browser's read", |a| {
        gate.reads() == 1 && a.tag_reads_in_flight() == 0
    });
    gate.close();

    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.tracks()[0].file_size, 90_000, "size from the browser's read");
    assert_eq!(app.metadata_cache.unreadable_size(&file), Some(90_000));
    assert!(app.metadata_cache.get(&file).is_none(), "no tags were read");
    let _ = fs::remove_dir_all(&dir);
}

/// A read that couldn't even find the file (the share went away for a moment) publishes no size
/// and leaves the path to be read again: the next add tries it once more, and gets the size.
#[test]
fn test_file_not_found_by_the_queue_read_is_read_again_on_the_next_add() {
    common::isolate();
    let dir = scratch_dir("stat_fails_once");
    let file = dir.join("back.ogg");
    let gate = Gate::default();
    let mut app = gated_app(&dir, &gate);
    let _open = OpenOnDrop(gate.clone());
    gate.open();
    // The first read finds nothing; the file is there for every read after it.
    let reads = gate.reads.clone();
    let restore = file.clone();
    app.set_tag_reader(Arc::new(move |path: &Path| {
        if reads.fetch_add(1, Ordering::SeqCst) > 0 && !restore.exists() {
            fs::write(&restore, vec![0x55u8; 70_000]).unwrap();
        }
        quickstream::metadata::extract_metadata(path)
    }));
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);

    tx.send(WebCommand::AddFile(file.clone())).unwrap();
    app.handle_web_commands();
    wait_until(&mut app, "the failed read to be reported", |a| {
        gate.reads() == 1 && !a.queue_metadata_attempted.contains(&file)
    });
    assert_eq!(app.queue.tracks()[0].file_size, 0);

    tx.send(WebCommand::AddFile(file.clone())).unwrap();
    app.handle_web_commands();
    wait_until(&mut app, "the size from the second read", |a| {
        a.queue.tracks().iter().all(|t| t.file_size == 70_000)
    });
    assert_eq!(app.queue.len(), 2);
    assert_eq!(gate.reads(), 2);
    let _ = fs::remove_dir_all(&dir);
}

/// An unreadable file queued again after it left the queue (`a`, `c`, `a`) gets the size found
/// the first time, from the metadata cache: the queue's workers read each path once a session.
#[test]
fn test_unreadable_file_queued_again_keeps_its_size() {
    common::isolate();
    let dir = scratch_dir("requeue");
    let file = dir.join("garbage.ogg");
    fs::write(&file, vec![0x55u8; 300_000]).unwrap();
    let gate = Gate::default();
    let mut app = gated_app(&dir, &gate);
    let _open = OpenOnDrop(gate.clone());

    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.queue.tracks()[0].file_size, 0, "looked up on the UI thread");
    gate.open();
    wait_until(&mut app, "the size", |a| a.queue.tracks()[0].file_size > 0);

    app.queue_focus = true;
    press(&mut app, KeyCode::Char('c'));
    assert!(app.queue.is_empty());
    app.queue_focus = false;
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.tracks()[0].file_size, 300_000, "size after re-adding");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_enter_on_a_file_row_queues_it_without_stat() {
    common::isolate();
    let dir = scratch_dir("enter");
    let song = dir.join("song.wav");
    write_silent_wav(&song, 4_000);
    let gate = Gate::default();
    let mut app = gated_app(&dir, &gate);
    let _open = OpenOnDrop(gate.clone());
    app.browser_cursor = app
        .browser_entries
        .iter()
        .position(|(_, p, _)| p == &song)
        .expect("song is listed");

    press(&mut app, KeyCode::Enter);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.tracks()[0].filepath, song);
    assert_eq!(app.queue.tracks()[0].file_size, 0, "looked up on the UI thread");

    gate.open();
    wait_until(&mut app, "the tag read", |a| a.queue.tracks()[0].file_size > 0);
    assert_eq!(app.queue.tracks()[0].file_size, size_on_disk(&song));
    let _ = fs::remove_dir_all(&dir);
}

/// The web thread has already resolved the path inside the allowed folders; the UI queues it as
/// given, without looking the file up again, so even one deleted in between is queued.
#[test]
fn test_web_add_file_queues_the_resolved_path_without_stat() {
    common::isolate();
    let dir = scratch_dir("web_add_file");
    let song = dir.join("song.wav");
    write_silent_wav(&song, 4_000);
    let gone = dir.join("gone.wav");
    let gate = Gate::default();
    let mut app = gated_app(&dir, &gate);
    let _open = OpenOnDrop(gate.clone());
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);

    tx.send(WebCommand::AddFile(song.clone())).unwrap();
    tx.send(WebCommand::AddFile(gone.clone())).unwrap();
    app.handle_web_commands();
    let paths: Vec<_> = app.queue.tracks().iter().map(|t| t.filepath.clone()).collect();
    assert_eq!(paths, vec![song.clone(), gone]);
    assert_eq!(app.queue.tracks()[0].file_size, 0, "looked up on the UI thread");

    gate.open();
    wait_until(&mut app, "the tag read", |a| a.queue.tracks()[0].file_size > 0);
    assert_eq!(app.queue.tracks()[0].file_size, size_on_disk(&song));
    let _ = fs::remove_dir_all(&dir);
}
