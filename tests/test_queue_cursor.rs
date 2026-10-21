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
    app.queue.set_current_index(2); // Song 3
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
    app.queue.set_current_index(1);
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

#[test]
fn test_inserting_tracks_keeps_the_current_track() {
    common::isolate();
    // Issue #52: the rest of a playlist is inserted around the track Enter already started.
    let mut queue = quickstream::queue::PlaybackQueue::new();
    queue.insert_tracks(0, vec![track(2)]);
    assert_eq!(
        queue.current_index(),
        0,
        "inserting into an empty queue starts at the top"
    );
    queue.insert_tracks(1, vec![track(3), track(4)]);
    assert_eq!(queue.current_index(), 0, "tracks after the current one leave it alone");
    queue.insert_tracks(0, vec![track(1)]);
    assert_eq!(queue.current_index(), 1, "tracks before the current one shift it along");
    assert_eq!(queue.current_track().unwrap().title, "Song 2");
    queue.insert_tracks(99, vec![track(5)]);
    let titles: Vec<&str> = queue.tracks().iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Song 1", "Song 2", "Song 3", "Song 4", "Song 5"]);
    assert_eq!(queue.current_index(), 1);
}

/// Titles of the queue, in order.
fn titles(app: &AppUI) -> Vec<String> {
    app.queue.tracks().iter().map(|t| t.title.clone()).collect()
}

fn press_z(app: &mut AppUI) {
    app.handle_input_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('z'),
        crossterm::event::KeyModifiers::NONE,
    ))
    .unwrap();
}

#[test]
fn test_shuffle_keeps_the_cursor_on_its_track() {
    common::isolate();
    // Issue #123: the shuffle reorders the queue, but the cursor kept its row number, so Enter
    // or `d` acted on whichever track had moved there. Every shuffle is checked: on a 12-track
    // queue a cursor left on its row lands on its own track again only by chance.
    let dir = temp_dir("shuffle_cursor");
    let mut app = app_with_queue(&dir, 12);
    app.queue.set_current_index(2);
    app.queue_cursor = 7;
    let mut on = titles(&app)[7].clone();
    for round in 0..8 {
        press_z(&mut app);
        assert!(app.status_message.contains("Queue shuffled"), "{}", app.status_message);
        assert_eq!(
            titles(&app)[app.queue_cursor],
            on,
            "round {round}: the cursor left its track; queue {:?}",
            titles(&app)
        );
        // Another row for the next round, so the rounds start from different rows (row 0, the
        // current track, among them).
        app.queue_cursor = (app.queue_cursor + 5) % app.queue.len();
        on = titles(&app)[app.queue_cursor].clone();
    }
}

#[test]
fn test_shuffle_keeps_the_cursor_on_the_same_copy_of_a_file_queued_twice() {
    common::isolate();
    // The same file queued five times: the cursor follows the entry it was on, not the first
    // copy of its file.
    let dir = temp_dir("shuffle_cursor_twice");
    let mut app = app_with_queue(&dir, 0);
    for i in 1..=10 {
        let mut meta = track(if i % 2 == 0 { 0 } else { i });
        // Same file, told apart by title only.
        meta.title = format!("Entry {}", i);
        app.queue.add_track(meta);
    }
    for round in 0..8 {
        let cursor = 1 + (round * 3) % 9;
        app.queue_cursor = cursor;
        let on = titles(&app)[cursor].clone();
        press_z(&mut app);
        assert_eq!(
            titles(&app)[app.queue_cursor],
            on,
            "round {round}: the cursor moved to another entry; queue {:?}",
            titles(&app)
        );
    }
}

#[test]
fn test_web_shuffle_keeps_the_cursor_on_its_track() {
    common::isolate();
    // The web dashboard's Shuffle (and the continuous stream's reshuffle on a wrap) shuffle
    // the way `z` does.
    let dir = temp_dir("shuffle_cursor_web");
    let mut app = app_with_queue(&dir, 12);
    let (tx, rx) = std::sync::mpsc::channel();
    app.web_cmd_rx = Some(rx);
    app.queue_cursor = 9;
    let mut on = titles(&app)[9].clone();
    for round in 0..8 {
        // Odd rounds: the stream's reshuffle, which has a command of its own.
        let cmd = if round % 2 == 0 {
            quickstream::web::WebCommand::ShuffleQueue
        } else {
            quickstream::web::WebCommand::StreamReshuffle
        };
        tx.send(cmd).unwrap();
        app.handle_web_commands();
        assert!(
            app.status_message.contains("Queue shuffled (web)"),
            "{}",
            app.status_message
        );
        assert_eq!(
            titles(&app)[app.queue_cursor],
            on,
            "round {round}: the cursor left its track; queue {:?}",
            titles(&app)
        );
        app.queue_cursor = (app.queue_cursor + 5) % app.queue.len();
        on = titles(&app)[app.queue_cursor].clone();
    }
}
