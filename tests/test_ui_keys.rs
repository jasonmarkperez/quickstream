//! Global letter keys as terminals send them (issue #99): crossterm reports an uppercase letter
//! with SHIFT (Shift held or Caps Lock on), so `N` (toggle Now Playing / Queue & Library) and `Z`
//! (shuffle the queue) work with SHIFT as well as without it, on every tab and pane, while
//! Ctrl / Alt chords of them do nothing.

mod common;

use std::fs;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use common::fake_player::FakePlayer;
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

/// An app browsing a temp dir of its own, with a fake player and five tracks queued.
fn keys_app(tag: &str) -> AppUI {
    let root = common::isolate();
    let dir = root.join(format!("ui_keys_{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    app.player = Box::new(FakePlayer::default());
    for i in 1..=5 {
        app.queue.add_track(AudioMetadata {
            filepath: dir.join(format!("{i:02}.flac")),
            filename: format!("{i:02}.flac"),
            title: format!("Track {i}"),
            ..Default::default()
        });
    }
    app
}

fn send(app: &mut AppUI, c: char, modifiers: KeyModifiers) {
    let quit = app
        .handle_input_key(KeyEvent::new(KeyCode::Char(c), modifiers))
        .unwrap();
    assert!(!quit, "{c:?} with {modifiers:?} must not quit");
}

/// `n` as typed, and `N` as Shift+N or Caps Lock sends it (SHIFT) and as some terminals do (NONE).
const N_KEYS: [(char, KeyModifiers); 3] = [
    ('n', KeyModifiers::NONE),
    ('N', KeyModifiers::SHIFT),
    ('N', KeyModifiers::NONE),
];
const Z_KEYS: [(char, KeyModifiers); 3] = [
    ('z', KeyModifiers::NONE),
    ('Z', KeyModifiers::SHIFT),
    ('Z', KeyModifiers::NONE),
];

#[test]
fn n_and_shift_n_toggle_now_playing_and_queue() {
    common::isolate();
    let mut app = keys_app("n");
    for (c, modifiers) in N_KEYS {
        app.active_tab = 0;
        send(&mut app, c, modifiers);
        assert_eq!(app.active_tab, 1, "{c:?} {modifiers:?} on Now Playing");
        send(&mut app, c, modifiers);
        assert_eq!(app.active_tab, 0, "{c:?} {modifiers:?} on Queue & Library");
        for tab in 2..5 {
            app.active_tab = tab;
            send(&mut app, c, modifiers);
            assert_eq!(app.active_tab, 0, "{c:?} {modifiers:?} on tab {}", tab + 1);
        }
    }
}

#[test]
fn z_and_shift_z_shuffle_the_queue_on_every_tab_and_pane() {
    common::isolate();
    let mut app = keys_app("z");
    let places = [(0, false), (1, false), (1, true), (2, false), (3, false), (4, false)];
    for (c, modifiers) in Z_KEYS {
        for (tab, queue_focus) in places {
            app.active_tab = tab;
            app.queue_focus = queue_focus;
            app.status_message.clear();
            let before = app.queue.revision();
            send(&mut app, c, modifiers);
            let at = format!("{c:?} {modifiers:?} on tab {} (queue pane {queue_focus})", tab + 1);
            assert!(
                app.status_message.contains("Queue shuffled"),
                "{at}: {:?}",
                app.status_message
            );
            assert_ne!(app.queue.revision(), before, "{at}: queue not shuffled");
            assert_eq!(app.queue.len(), 5, "{at}");
        }
    }
}

#[test]
fn ctrl_and_alt_chords_of_n_and_z_do_nothing() {
    common::isolate();
    let mut app = keys_app("chords");
    let places = [(0, false), (1, false), (1, true), (2, false), (3, false), (4, false)];
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        for c in ['n', 'N', 'z', 'Z'] {
            for (tab, queue_focus) in places {
                app.active_tab = tab;
                app.queue_focus = queue_focus;
                app.status_message.clear();
                let before = app.queue.revision();
                send(&mut app, c, modifiers);
                let at = format!("{c:?} {modifiers:?} on tab {} (queue pane {queue_focus})", tab + 1);
                assert_eq!(app.active_tab, tab, "{at}: switched tabs");
                assert_eq!(app.queue.revision(), before, "{at}: queue changed");
                assert!(
                    !app.status_message.contains("shuffled"),
                    "{at}: {:?}",
                    app.status_message
                );
            }
        }
    }
}
