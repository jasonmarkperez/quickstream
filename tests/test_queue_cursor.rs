//! The queue cursor stays inside the queue after removals, clears and reshuffles (issue #16).

mod common;

use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_queue_cursor_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn track(n: usize) -> AudioMetadata {
    AudioMetadata {
        filepath: PathBuf::from(format!("/qs-test-nonexistent/song{}.mp3", n)),
        filename: format!("song{}.mp3", n),
        title: format!("Song {}", n),
        artist: "Artist".to_string(),
        duration_seconds: 100.0,
        ..Default::default()
    }
}

fn app_with_queue(dir: &Path, n: usize) -> AppUI {
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(dir.to_path_buf()), settings_mgr);
    for i in 1..=n {
        app.queue.add_track(track(i));
    }
    app.active_tab = 1;
    app.queue_focus = true;
    app
}

#[test]
fn test_removing_last_queue_row_clamps_cursor() {
    common::isolate();
    let dir = temp_dir("remove_last");
    let mut app = app_with_queue(&dir, 3);
    app.queue_cursor = 2;
    app.handle_tab_key(KeyCode::Char('d'));
    assert_eq!(app.queue.len(), 2);
    assert_eq!(app.queue_cursor, 1);

    app.handle_tab_key(KeyCode::Char('d'));
    app.handle_tab_key(KeyCode::Char('d'));
    assert!(app.queue.is_empty());
    assert_eq!(app.queue_cursor, 0);
}

#[test]
fn test_removing_row_before_current_keeps_current_track() {
    common::isolate();
    let dir = temp_dir("remove_before");
    let mut app = app_with_queue(&dir, 4);
    app.queue.current_index = 2; // Song 3
    app.queue_cursor = 0;
    app.handle_tab_key(KeyCode::Char('d'));
    assert_eq!(app.queue.current_track().unwrap().title, "Song 3");
}

#[test]
fn test_clearing_queue_resets_cursor() {
    common::isolate();
    let dir = temp_dir("clear");
    let mut app = app_with_queue(&dir, 5);
    app.queue_cursor = 4;
    app.handle_tab_key(KeyCode::Char('c'));
    assert!(app.queue.is_empty());
    assert_eq!(app.queue_cursor, 0);
}

#[test]
fn test_reshuffle_truncation_clamps_cursor() {
    common::isolate();
    let dir = temp_dir("reshuffle");
    let mut app = app_with_queue(&dir, 5);
    app.queue.current_index = 1;
    app.queue_cursor = 4;
    app.reshuffle_autopilot(dir.clone());
    assert_eq!(app.queue.len(), 2);
    assert!(
        app.queue_cursor < app.queue.len(),
        "cursor {} past queue of {}",
        app.queue_cursor,
        app.queue.len()
    );
}
