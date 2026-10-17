//! Tab 5 (`[5] Playlists`): tab switching, two-pane navigation, rendering and the modal hook.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use quickstream::paths;
use quickstream::playlist::{NameError, Playlist, PlaylistEntry};
use quickstream::playlist_store::StoreError;
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{NamePurpose, PlaylistModal};
use quickstream::ui::AppUI;

/// A fresh temp dir for one test. Also empties the playlists folder, which every test in this
/// binary shares through the one isolated config dir.
fn fresh_dirs(tag: &str) -> PathBuf {
    let _ = fs::remove_dir_all(paths::get_playlists_dir());
    let temp_dir = std::env::temp_dir().join(format!("quickstream_pl_ui_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    temp_dir
}

fn entry(dir: &Path, file: &str, artist: &str, title: &str, secs: i64) -> PlaylistEntry {
    PlaylistEntry {
        path: dir.join(file),
        duration_secs: secs,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

/// Write a playlist file straight into the playlists folder, as if saved by an earlier run.
fn write_playlist(name: &str, entries: Vec<PlaylistEntry>) {
    let dir = paths::get_playlists_dir();
    fs::create_dir_all(&dir).unwrap();
    let playlist = Playlist {
        name: name.to_string(),
        entries,
    };
    fs::write(dir.join(format!("{}.m3u8", name)), playlist.to_m3u8()).unwrap();
}

fn new_app(temp_dir: &Path) -> AppUI {
    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    AppUI::new(Some(temp_dir.to_path_buf()), settings_mgr)
}

/// Poll until the startup load has reached the store (bounded, ~2s).
fn wait_loaded(app: &mut AppUI) {
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("playlists did not finish loading");
}

fn press(app: &mut AppUI, code: KeyCode) -> bool {
    app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
}

fn render(app: &mut AppUI, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn test_key_5_and_tab_cycling_cover_five_tabs() {
    common::isolate();
    let temp_dir = fresh_dirs("tabs");
    let mut app = new_app(&temp_dir);

    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.active_tab, 4);

    // Tab wraps from the playlists tab to Now Playing, BackTab wraps back.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.active_tab, 0);
    app.handle_input_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        .unwrap();
    assert_eq!(app.active_tab, 4);

    // Settings -> Playlists with Tab.
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.active_tab, 4);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_shows_loading_then_empty_state() {
    common::isolate();
    let temp_dir = fresh_dirs("empty");
    let mut app = new_app(&temp_dir);
    app.active_tab = 4;

    // Nothing has been polled yet, so the store is still loading.
    let view = render(&mut app, 120, 30);
    assert!(view.contains("[5] Playlists"), "tab bar should list tab 5:\n{}", view);
    assert!(view.contains("Loading playlists…"), "{}", view);

    wait_loaded(&mut app);
    let view = render(&mut app, 120, 30);
    assert!(!view.contains("Loading playlists…"), "{}", view);
    assert!(view.contains("No playlists yet."), "{}", view);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_startup_load_reserves_the_names_of_skipped_files() {
    common::isolate();
    let temp_dir = fresh_dirs("reserved");
    // Spec Section 1: a file the load skips is never rewritten. `Latin.m3u8` is not UTF-8 and
    // `Mix.M3U8` has an upper-case extension. On a case-insensitive disk `latin.m3u8` and
    // `mix.m3u8` are those same files.
    let dir = paths::get_playlists_dir();
    fs::create_dir_all(&dir).unwrap();
    let latin1: &[u8] = b"#EXTM3U\n/music/caf\xe9.flac\n";
    fs::write(dir.join("Latin.m3u8"), latin1).unwrap();
    let mix = "#EXTM3U\n/music/mix.flac\n";
    fs::write(dir.join("Mix.M3U8"), mix).unwrap();

    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    assert!(app.playlists.store.playlists().is_empty());

    assert_eq!(
        app.playlists.store.create("latin"),
        Err(StoreError::Name(NameError::Duplicate("Latin".to_string())))
    );
    assert_eq!(
        app.playlists.store.create("MIX"),
        Err(StoreError::Name(NameError::Duplicate("Mix".to_string())))
    );
    assert!(app.playlists.store.playlists().is_empty());

    assert!(app.persistence.flush());
    assert_eq!(fs::read(dir.join("Latin.m3u8")).unwrap(), latin1);
    assert_eq!(fs::read_to_string(dir.join("Mix.M3U8")).unwrap(), mix);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_renders_rows_titles_and_empty_playlist() {
    common::isolate();
    let temp_dir = fresh_dirs("render");
    write_playlist(
        "Road Trip",
        vec![
            entry(&temp_dir, "01 Airbag.flac", "Radiohead", "Airbag", 284),
            entry(&temp_dir, "02 Paranoid Android.flac", "", "Paranoid Android", -1),
        ],
    );
    write_playlist("chill", Vec::new());
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    // Sorted case-insensitively: "chill" comes first and is selected.
    let view = render(&mut app, 120, 30);
    assert!(view.contains("chill  (0 tracks)"), "{}", view);
    assert!(view.contains("Road Trip  (2 tracks)"), "{}", view);
    assert!(
        view.contains("Empty. Press L on a track in tab 2 to add it."),
        "{}",
        view
    );

    // Down selects Road Trip; the unknown duration is left out of the total.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Road Trip"));
    let view = render(&mut app, 120, 30);
    assert!(view.contains("ROAD TRIP (2 tracks, 4:44)"), "{}", view);
    assert!(view.contains("1. Radiohead - Airbag  4:44"), "{}", view);
    assert!(view.contains("2. Paranoid Android  -:--"), "{}", view);

    // A small terminal must not panic.
    render(&mut app, 40, 12);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_focus_and_wraparound_navigation() {
    common::isolate();
    let temp_dir = fresh_dirs("nav");
    write_playlist("Alpha", vec![entry(&temp_dir, "a1.flac", "A", "One", 60)]);
    write_playlist(
        "Bravo",
        vec![
            entry(&temp_dir, "b1.flac", "B", "One", 60),
            entry(&temp_dir, "b2.flac", "B", "Two", 60),
            entry(&temp_dir, "b3.flac", "B", "Three", 60),
        ],
    );
    write_playlist("Charlie", Vec::new());
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.playlists.list_cursor, 0);
    assert!(!app.playlists.tracks_focus);

    // Left pane wraps both ways (k / Up at the top, j / Down at the bottom).
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.list_cursor, 2);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.list_cursor, 0);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.playlists.list_cursor, 2);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Bravo"));

    // Enter on the left pane focuses the tracks pane; h / l switch back and forth.
    press(&mut app, KeyCode::Enter);
    assert!(app.playlists.tracks_focus);
    press(&mut app, KeyCode::Char('h'));
    assert!(!app.playlists.tracks_focus);
    press(&mut app, KeyCode::Char('l'));
    assert!(app.playlists.tracks_focus);

    // Tracks pane wraps both ways and leaves the playlist selection alone.
    assert_eq!(app.playlists.track_cursor, 0);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.track_cursor, 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.track_cursor, 0);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.track_cursor, 1);
    assert_eq!(app.playlists.list_cursor, 1);

    // Moving to another playlist starts its tracks at the top.
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Charlie"));
    assert_eq!(app.playlists.track_cursor, 0);

    // The keys only act on tab 5: on tab 4 (Settings) 'j' moves the settings cursor instead.
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.list_cursor, 2);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_cursors_clamp_when_lists_shrink() {
    common::isolate();
    let temp_dir = fresh_dirs("clamp");
    write_playlist("a", vec![entry(&temp_dir, "a1.flac", "A", "One", 60)]);
    write_playlist(
        "b",
        vec![
            entry(&temp_dir, "b1.flac", "B", "One", 60),
            entry(&temp_dir, "b2.flac", "B", "Two", 60),
            entry(&temp_dir, "b3.flac", "B", "Three", 60),
        ],
    );
    write_playlist("c", vec![entry(&temp_dir, "c1.flac", "C", "One", 60)]);
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    // Select the last playlist, then remove it from the store.
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.list_cursor, 2);
    app.playlists.store.delete("c").unwrap();
    app.poll_playlists();
    assert_eq!(app.playlists.list_cursor, 1);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("b"));

    // Select the last track of "b", then remove it.
    press(&mut app, KeyCode::Char('l'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.track_cursor, 2);
    app.playlists.store.remove("b", 2).unwrap();
    let _ = render(&mut app, 120, 30);
    assert_eq!(app.playlists.track_cursor, 1);

    // Everything gone: both cursors back at 0 and the empty state renders.
    app.playlists.store.delete("a").unwrap();
    app.playlists.store.delete("b").unwrap();
    app.poll_playlists();
    assert_eq!(app.playlists.list_cursor, 0);
    assert_eq!(app.playlists.track_cursor, 0);
    assert_eq!(app.playlists.selected_name(), None);
    let view = render(&mut app, 120, 30);
    assert!(view.contains("No playlists yet."), "{}", view);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlist_modal_takes_keys_before_global_hotkeys() {
    common::isolate();
    let temp_dir = fresh_dirs("modal");
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    app.playlists.modal = Some(PlaylistModal::NamePrompt {
        purpose: NamePurpose::Create,
        input: String::new(),
        error: None,
    });
    let view = render(&mut app, 120, 30);
    assert!(view.contains("NEW PLAYLIST"), "{}", view);

    // Neither a tab key nor q (quit) reaches the global hotkeys while the modal is open.
    assert!(!press(&mut app, KeyCode::Char('1')));
    assert_eq!(app.active_tab, 4);
    assert!(!press(&mut app, KeyCode::Char('q')));
    assert!(app.playlists.modal.is_some());

    // Esc closes it; afterwards keys reach the tab again.
    assert!(!press(&mut app, KeyCode::Esc));
    assert!(app.playlists.modal.is_none());
    press(&mut app, KeyCode::Char('1'));
    assert_eq!(app.active_tab, 0);

    // Ctrl+C quits from inside the modal, like the other modals.
    app.playlists.modal = Some(PlaylistModal::ConfirmDelete {
        name: "Road Trip".to_string(),
        count: 24,
    });
    let view = render(&mut app, 120, 30);
    assert!(view.contains("Delete \"Road Trip\" (24 tracks)? y/n"), "{}", view);
    let quit = app
        .handle_input_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(quit);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_lists_tab_5_navigation() {
    common::isolate();
    let temp_dir = fresh_dirs("help");
    let mut app = new_app(&temp_dir);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.show_help_modal);

    let view = render(&mut app, 100, 40);
    assert!(view.contains("1 - 5"), "{}", view);
    assert!(view.contains("PLAYLISTS (TAB 5)"), "{}", view);
    assert!(view.contains("Focus Playlists"), "{}", view);
    assert!(view.contains("Open playlist's tracks"), "{}", view);

    // The 20-row terminal the existing help test uses still renders without panicking.
    render(&mut app, 70, 20);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_tab_bar_shows_all_five_labels_at_80_columns() {
    common::isolate();
    let temp_dir = fresh_dirs("tabbar80");
    let mut app = new_app(&temp_dir);

    // Every tab selection renders the full bar at a standard 80-column width.
    for tab in 0..5 {
        app.active_tab = tab;
        let out = render(&mut app, 80, 30);
        let first_line = out.lines().next().unwrap_or("").to_string();
        println!("tab {tab} bar: {first_line}");
        for label in [
            "[1] Now Playing",
            "[2] Queue & Library",
            "[3] Devices",
            "[4] Settings",
            "[5] Playlists",
        ] {
            assert!(
                first_line.contains(label),
                "missing {label:?} in tab bar (tab {tab}): {first_line:?}"
            );
        }
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_tab_5_label_is_highlighted_at_80_columns_after_pressing_5() {
    common::isolate();
    let temp_dir = fresh_dirs("tabbar80hl");
    let mut app = new_app(&temp_dir);

    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.active_tab, 4);

    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let buffer = terminal.backend().buffer();

    let row: String = (0..80).map(|x| buffer[(x, 0)].symbol().to_string()).collect();
    let label = "[5] Playlists";
    let start = row
        .chars()
        .collect::<Vec<_>>()
        .windows(label.chars().count())
        .position(|w| w.iter().collect::<String>() == label)
        .unwrap_or_else(|| panic!("{label:?} not on the 80-column tab bar: {row:?}"));
    let end = start + label.chars().count();
    assert!(end <= 80, "{label:?} clipped at the right edge: {row:?}");

    for x in start..end {
        let cell = &buffer[(x as u16, 0)];
        assert_eq!(
            cell.fg,
            ratatui::style::Color::Cyan,
            "column {x} of {label:?} is not highlighted"
        );
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::BOLD),
            "column {x} of {label:?} is not bold"
        );
    }

    let _ = fs::remove_dir_all(&temp_dir);
}
