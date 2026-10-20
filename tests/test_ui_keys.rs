//! Character keys as terminals send them (issues #99, #119): crossterm reports an uppercase
//! letter or a shifted symbol with SHIFT (Shift held, Caps Lock on, or a CSI-u / modifyOtherKeys
//! terminal), and `ui::normalize_key` drops it before any handler sees the key, so `N` / `Z` /
//! `?` / `+` and the uppercase tab keys work either way, on every tab and pane, and typed text
//! keeps its case. A Ctrl / Alt chord of a letter is the lowercase letter with only its chord
//! modifiers: the bound chords (`Ctrl+C`, paging, search's `Ctrl+A` / `Ctrl+P` / `Ctrl+N`) work
//! with Shift too, and every other chord of a letter does nothing.

mod common;

use std::fs;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use common::fake_player::FakePlayer;
use quickstream::metadata::AudioMetadata;
use quickstream::paths;
use quickstream::playlist::PlaylistEntry;
use quickstream::settings::{setting, SettingsManager, SETTING_DISPLAY_ORDER};
use quickstream::ui::playlists::PlaylistModal;
use quickstream::ui::{normalize_key, AppUI};

/// An app browsing a temp dir of its own, with a fake player, five tracks queued and a loaded
/// playlists store holding one playlist (`Keys`, two tracks). Tests in this binary share one
/// isolated config dir, so the playlists folder is removed first.
fn keys_app(tag: &str) -> AppUI {
    let root = common::isolate();
    let dir = root.join(format!("ui_keys_{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let _ = fs::remove_dir_all(paths::get_playlists_dir());
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
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.playlists.store.is_loaded(), "playlists did not load within 2s");
    let name = app.playlists.store.create("Keys").unwrap();
    let entries = (1..=2)
        .map(|i| PlaylistEntry::from_path(dir.join(format!("{i:02}.flac")).as_path()))
        .collect();
    app.playlists.store.add(&name, entries).unwrap();
    app
}

/// Every tab and pane: tab [2]'s LIBRARY and QUEUE, tab [5]'s PLAYLISTS and tracks.
const PLACES: [(usize, bool); 7] = [
    (0, false),
    (1, false),
    (1, true),
    (2, false),
    (3, false),
    (4, false),
    (4, true),
];

/// Go to `tab` with its second pane (QUEUE on tab [2], tracks on tab [5]) focused or not.
fn go_to(app: &mut AppUI, tab: usize, second_pane: bool) {
    app.active_tab = tab;
    app.queue_focus = tab == 1 && second_pane;
    app.playlists.tracks_focus = tab == 4 && second_pane;
    app.playlists.list_cursor = 0;
}

fn place(tab: usize, second_pane: bool) -> String {
    match (tab, second_pane) {
        (1, true) => "tab 2 (queue pane)".to_string(),
        (4, true) => "tab 5 (tracks pane)".to_string(),
        _ => format!("tab {}", tab + 1),
    }
}

fn key(c: char, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), modifiers)
}

/// Send one key; returns true when the app asked to quit.
fn press(app: &mut AppUI, c: char, modifiers: KeyModifiers) -> bool {
    app.handle_input_key(key(c, modifiers)).unwrap()
}

fn send(app: &mut AppUI, c: char, modifiers: KeyModifiers) {
    assert!(!press(app, c, modifiers), "{c:?} with {modifiers:?} must not quit");
}

/// `n` as typed, and `N` as Shift+N or Caps Lock sends it (SHIFT), as some terminals do (NONE)
/// and as a CSI-u terminal reports Shift+N (`n` with SHIFT).
const N_KEYS: [(char, KeyModifiers); 4] = [
    ('n', KeyModifiers::NONE),
    ('N', KeyModifiers::SHIFT),
    ('N', KeyModifiers::NONE),
    ('n', KeyModifiers::SHIFT),
];
const Z_KEYS: [(char, KeyModifiers); 4] = [
    ('z', KeyModifiers::NONE),
    ('Z', KeyModifiers::SHIFT),
    ('Z', KeyModifiers::NONE),
    ('z', KeyModifiers::SHIFT),
];
const CHORDS: [KeyModifiers; 4] = [
    KeyModifiers::CONTROL,
    KeyModifiers::ALT,
    KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
    KeyModifiers::ALT.union(KeyModifiers::SHIFT),
];

#[test]
fn normalize_key_drops_shift_from_characters() {
    common::isolate();
    let cases = [
        (key('N', KeyModifiers::SHIFT), key('N', KeyModifiers::NONE)),
        (key('n', KeyModifiers::SHIFT), key('N', KeyModifiers::NONE)),
        (key('N', KeyModifiers::NONE), key('N', KeyModifiers::NONE)),
        (key('n', KeyModifiers::NONE), key('n', KeyModifiers::NONE)),
        (key('?', KeyModifiers::SHIFT), key('?', KeyModifiers::NONE)),
        (key('+', KeyModifiers::SHIFT), key('+', KeyModifiers::NONE)),
        (key('ß', KeyModifiers::SHIFT), key('ß', KeyModifiers::NONE)),
        (key('é', KeyModifiers::SHIFT), key('É', KeyModifiers::NONE)),
        // A chord is its lowercase letter with only its chord modifiers.
        (key('c', KeyModifiers::CONTROL), key('c', KeyModifiers::CONTROL)),
        (
            key('C', KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            key('c', KeyModifiers::CONTROL),
        ),
        (key('N', KeyModifiers::ALT), key('n', KeyModifiers::ALT)),
        (
            key('>', KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            key('>', KeyModifiers::CONTROL),
        ),
    ];
    // `KeyEvent`'s own `==` treats `N` and `N` with SHIFT as equal, so compare the fields.
    let parts = |k: KeyEvent| (k.code, k.modifiers, k.kind, k.state);
    for (sent, expected) in cases {
        assert_eq!(parts(normalize_key(sent)), parts(expected), "{sent:?}");
    }
    // Other keys keep their modifiers.
    for sent in [
        KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
    ] {
        assert_eq!(parts(normalize_key(sent)), parts(sent), "{sent:?}");
    }
}

#[test]
fn n_and_shift_n_toggle_now_playing_and_queue_on_every_tab_and_pane() {
    common::isolate();
    let mut app = keys_app("n");
    for (c, modifiers) in N_KEYS {
        for (tab, second_pane) in PLACES {
            go_to(&mut app, tab, second_pane);
            send(&mut app, c, modifiers);
            let expected = if tab == 0 { 1 } else { 0 };
            let at = place(tab, second_pane);
            assert_eq!(app.active_tab, expected, "{c:?} {modifiers:?} on {at}");
        }
    }
}

#[test]
fn z_and_shift_z_shuffle_the_queue_on_every_tab_and_pane() {
    common::isolate();
    let mut app = keys_app("z");
    for (c, modifiers) in Z_KEYS {
        for (tab, second_pane) in PLACES {
            go_to(&mut app, tab, second_pane);
            app.status_message.clear();
            let before = app.queue.revision();
            send(&mut app, c, modifiers);
            let at = format!("{c:?} {modifiers:?} on {}", place(tab, second_pane));
            assert!(
                app.status_message.contains("Queue shuffled"),
                "{at}: {:?}",
                app.status_message
            );
            assert_ne!(app.queue.revision(), before, "{at}: queue not shuffled");
            assert_eq!(app.queue.len(), 5, "{at}");
            assert_eq!(app.active_tab, tab, "{at}: switched tabs");
        }
    }
}

#[test]
fn ctrl_and_alt_chords_of_n_and_z_do_nothing() {
    common::isolate();
    let mut app = keys_app("chords");
    for modifiers in CHORDS {
        for c in ['n', 'N', 'z', 'Z'] {
            for (tab, second_pane) in PLACES {
                go_to(&mut app, tab, second_pane);
                app.status_message.clear();
                let before = app.queue.revision();
                send(&mut app, c, modifiers);
                let at = format!("{c:?} {modifiers:?} on {}", place(tab, second_pane));
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

/// `P` (reshuffle autopilot), `M` (mix picker) and `L` (add picker) follow the same rule as `N`
/// and `Z`: with or without SHIFT they act, and their Ctrl / Alt chords do nothing.
#[test]
fn ctrl_and_alt_chords_of_p_m_and_l_do_nothing() {
    common::isolate();
    let mut app = keys_app("pml");
    for modifiers in CHORDS {
        for c in ['p', 'P', 'm', 'M', 'l', 'L'] {
            for (tab, second_pane) in PLACES {
                go_to(&mut app, tab, second_pane);
                app.status_message = "before".to_string();
                send(&mut app, c, modifiers);
                let at = format!("{c:?} {modifiers:?} on {}", place(tab, second_pane));
                assert!(!app.is_autopilot_scanning, "{at}: started autopilot");
                assert!(!app.show_mix_picker_modal, "{at}: opened the mix picker");
                assert!(app.playlists.modal.is_none(), "{at}: opened a playlist modal");
                assert_eq!(app.status_message, "before", "{at}");
            }
        }
    }
}

#[test]
fn shift_m_and_l_open_their_pickers_however_shift_is_reported() {
    common::isolate();
    let mut app = keys_app("ml");
    for (c, modifiers) in [
        ('M', KeyModifiers::SHIFT),
        ('M', KeyModifiers::NONE),
        ('m', KeyModifiers::SHIFT),
    ] {
        go_to(&mut app, 0, false);
        send(&mut app, c, modifiers);
        assert!(app.show_mix_picker_modal, "{c:?} {modifiers:?} opens the mix picker");
        send(&mut app, c, modifiers);
        assert!(!app.show_mix_picker_modal, "{c:?} {modifiers:?} closes the mix picker");
    }
    app.player = Box::new(FakePlayer::playing_at(10.0, 100.0));
    for (c, modifiers) in [
        ('L', KeyModifiers::SHIFT),
        ('L', KeyModifiers::NONE),
        ('l', KeyModifiers::SHIFT),
    ] {
        go_to(&mut app, 0, false);
        app.playlists.modal = None;
        send(&mut app, c, modifiers);
        assert!(
            matches!(app.playlists.modal, Some(PlaylistModal::AddPicker { .. })),
            "{c:?} {modifiers:?} opens the add picker: {:?}",
            app.status_message
        );
    }
    app.playlists.modal = None;
}

/// Terminals that report shifted symbols with SHIFT (kitty / WezTerm CSI-u, xterm
/// modifyOtherKeys=2): `?` opens and closes the help, `+` raises the volume, `<` / `>` seek.
#[test]
fn shifted_symbols_act_with_or_without_shift() {
    common::isolate();
    let mut app = keys_app("symbols");
    for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
        for (tab, second_pane) in PLACES {
            go_to(&mut app, tab, second_pane);
            let at = format!("{modifiers:?} on {}", place(tab, second_pane));
            send(&mut app, '?', modifiers);
            assert!(app.show_help_modal, "'?' {at} opens the help");
            send(&mut app, '?', modifiers);
            assert!(!app.show_help_modal, "'?' {at} closes the help");
        }

        go_to(&mut app, 0, false);
        let step = app.settings_mgr.settings.volume_step;
        for c in ['+', '='] {
            app.player.set_volume(40).unwrap();
            send(&mut app, c, modifiers);
            assert_eq!(app.player.volume(), 40 + step, "{c:?} {modifiers:?} raises the volume");
        }
        app.player.set_volume(40).unwrap();
        send(&mut app, '-', modifiers);
        assert_eq!(app.player.volume(), 40 - step, "'-' {modifiers:?} lowers the volume");

        for (c, sign) in [('>', '+'), ('<', '-')] {
            app.status_message.clear();
            send(&mut app, c, modifiers);
            assert!(
                app.status_message.starts_with(&format!("Seek {sign}")),
                "{c:?} {modifiers:?} seeks: {:?}",
                app.status_message
            );
        }
    }
}

/// Uppercase tab keys work whichever way Shift is reported: `⇧J` jumps to the last setting on
/// tab [4] (`j` without Shift steps one).
#[test]
fn uppercase_tab_keys_act_however_shift_is_reported() {
    common::isolate();
    let mut app = keys_app("tabkeys");
    let last = SETTING_DISPLAY_ORDER[SETTING_DISPLAY_ORDER.len() - 1];
    for (c, modifiers) in [
        ('J', KeyModifiers::SHIFT),
        ('J', KeyModifiers::NONE),
        ('j', KeyModifiers::SHIFT),
    ] {
        go_to(&mut app, 3, false);
        app.settings_cursor = SETTING_DISPLAY_ORDER[0];
        send(&mut app, c, modifiers);
        assert_eq!(app.settings_cursor, last, "{c:?} {modifiers:?} on tab 4");
    }
    // Its chords do nothing.
    for modifiers in CHORDS {
        app.settings_cursor = SETTING_DISPLAY_ORDER[0];
        send(&mut app, 'J', modifiers);
        assert_eq!(
            app.settings_cursor, SETTING_DISPLAY_ORDER[0],
            "'J' {modifiers:?} on tab 4"
        );
    }
}

/// Bound chords keep working when Shift is held too (a terminal that tells them apart reports
/// it; one that can't sends the plain chord).
#[test]
fn bound_chords_work_with_shift_held() {
    common::isolate();
    let mut app = keys_app("bound");
    go_to(&mut app, 0, false);
    assert!(
        press(&mut app, 'C', KeyModifiers::CONTROL | KeyModifiers::SHIFT),
        "Ctrl+Shift+C quits"
    );
    let mut app = keys_app("bound2");
    go_to(&mut app, 0, false);
    assert!(press(&mut app, 'c', KeyModifiers::CONTROL), "Ctrl+C quits");
}

/// Typed text keeps the case and symbols the terminal reports, with SHIFT or without, and a
/// chord types nothing: the search query, the spotlight, the name prompt and the home editor.
#[test]
fn typed_text_keeps_case_and_symbols_with_shift() {
    common::isolate();
    let typed = [
        ('A', KeyModifiers::SHIFT),
        ('b', KeyModifiers::SHIFT),
        ('c', KeyModifiers::NONE),
        ('D', KeyModifiers::NONE),
        ('?', KeyModifiers::SHIFT),
        ('~', KeyModifiers::SHIFT),
        ('q', KeyModifiers::NONE),
        ('x', KeyModifiers::CONTROL),
        ('Y', KeyModifiers::ALT | KeyModifiers::SHIFT),
    ];
    let expected = "ABcD?~q";
    let mut app = keys_app("typed");

    // Search (`f` on tab 1).
    go_to(&mut app, 0, false);
    send(&mut app, 'f', KeyModifiers::NONE);
    assert!(app.search_modal.is_some(), "f opens the search");
    for (c, modifiers) in typed {
        send(&mut app, c, modifiers);
    }
    assert_eq!(app.search_modal.as_ref().unwrap().query, expected, "search query");
    send(&mut app, 'L', KeyModifiers::SHIFT);
    assert!(app.playlists.modal.is_none(), "L in the search is text");
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(app.search_modal.is_none(), "Esc closes the search");

    // Spotlight (`/` in the LIBRARY pane).
    go_to(&mut app, 1, false);
    send(&mut app, '/', KeyModifiers::NONE);
    assert_eq!(app.browser_spotlight.as_deref(), Some(""), "/ opens the spotlight");
    for (c, modifiers) in typed {
        send(&mut app, c, modifiers);
    }
    assert_eq!(app.browser_spotlight.as_deref(), Some(expected), "spotlight query");
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(app.browser_spotlight.is_none(), "Esc closes the spotlight");

    // Name prompt (`c` in the PLAYLISTS pane).
    go_to(&mut app, 4, false);
    send(&mut app, 'c', KeyModifiers::NONE);
    for (c, modifiers) in typed {
        send(&mut app, c, modifiers);
    }
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { input, .. }) => assert_eq!(input, expected, "name prompt"),
        other => panic!("expected the name prompt, got {other:?}"),
    }
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();

    // Startup home editor (Enter on Startup Home Directory).
    go_to(&mut app, 3, false);
    app.settings_cursor = setting::HOME_DIR;
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    for (c, modifiers) in typed {
        send(&mut app, c, modifiers);
    }
    assert_eq!(app.editing_home_dir.as_deref(), Some(expected), "home editor");
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
}
