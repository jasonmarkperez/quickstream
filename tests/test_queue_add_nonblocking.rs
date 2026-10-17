//! Adding tracks must not read audio files on the UI thread (issue #2).
//!
//! Each test uses a named pipe with an audio extension: opening it blocks until a writer
//! connects, like a slow network share. A helper thread connects as the writer after
//! `SLOW_FILE_DELAY`, so any synchronous metadata read on the UI thread takes at least that long.

mod common;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::WebCommand;

const SLOW_FILE_DELAY: Duration = Duration::from_millis(1500);
const UI_BUDGET: Duration = Duration::from_millis(300);

fn make_slow_audio_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let status = std::process::Command::new("mkfifo").arg(&path).status().unwrap();
    assert!(status.success());
    let writer_path = path.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SLOW_FILE_DELAY);
        // Blocks until a reader opens the pipe; closing it gives the reader EOF.
        let _ = std::fs::OpenOptions::new().write(true).open(&writer_path);
    });
    path
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_with_web_commands() -> (AppUI, mpsc::Sender<WebCommand>) {
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(None, settings_mgr);
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);
    (app, tx)
}

#[test]
fn test_web_add_file_does_not_read_metadata_on_ui_thread() {
    common::isolate();
    let dir = temp_dir("add_file");
    let slow = make_slow_audio_file(&dir, "slow.mp3");
    let (mut app, tx) = app_with_web_commands();

    tx.send(WebCommand::AddFile(slow.clone())).unwrap();
    let start = Instant::now();
    app.handle_web_commands();
    let elapsed = start.elapsed();

    assert!(elapsed < UI_BUDGET, "UI thread blocked for {:?}", elapsed);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.tracks[0].filepath, slow);
}

#[test]
fn test_web_add_folder_does_not_block_ui_thread() {
    common::isolate();
    let dir = temp_dir("add_folder");
    let slow = make_slow_audio_file(&dir, "slow.mp3");
    let (mut app, tx) = app_with_web_commands();

    tx.send(WebCommand::AddFolder(dir.clone())).unwrap();
    let start = Instant::now();
    app.handle_web_commands();
    let elapsed = start.elapsed();
    assert!(elapsed < UI_BUDGET, "UI thread blocked for {:?}", elapsed);

    let deadline = Instant::now() + Duration::from_secs(3);
    while app.queue.is_empty() && Instant::now() < deadline {
        let tick = Instant::now();
        app.poll_pending_queue_adds();
        assert!(tick.elapsed() < UI_BUDGET, "poll blocked for {:?}", tick.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.tracks[0].filepath, slow);
}
