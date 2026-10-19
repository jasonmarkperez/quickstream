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

/// The first row of `buffer` containing `text`.
fn row_with(buffer: &ratatui::buffer::Buffer, text: &str) -> u16 {
    common::tui::find_row(buffer, text)
        .unwrap_or_else(|| panic!("{text:?} not drawn:\n{}", common::tui::rows(buffer).join("\n")))
}

/// The first row whose PLAYLISTS pane part (columns 2–45 at 120 columns) contains `text`.
fn pane_row(buffer: &ratatui::buffer::Buffer, text: &str) -> u16 {
    (0..buffer.area.height)
        .find(|&y| common::tui::cells(buffer, y)[2..46].concat().contains(text))
        .unwrap_or_else(|| panic!("{text:?} not in the pane:\n{}", common::tui::rows(buffer).join("\n")))
}

/// Whether the PLAYLISTS pane's part of row `y` (columns 2–45 at 120 columns) ends with `text`.
fn pane_ends_with(buffer: &ratatui::buffer::Buffer, y: u16, text: &str) -> bool {
    common::tui::cells(buffer, y)[2..46].concat().trim_end().ends_with(text)
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
    assert!(view.contains("5 PLAYLISTS"), "tab row should list tab 5:\n{}", view);
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
        Err(StoreError::Name(NameError::Reserved("Latin".to_string())))
    );
    assert_eq!(
        app.playlists.store.create("MIX"),
        Err(StoreError::Name(NameError::Reserved("Mix".to_string())))
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

    // Sorted case-insensitively: "chill" comes first and is selected. Each row is the name
    // (UPPERCASE) and its track count against the panel's right edge.
    let buffer = common::tui::render(&mut app, 120, 30);
    let view = common::tui::rows(&buffer).join("\n");
    let chill = pane_row(&buffer, "CHILL");
    let road_trip = pane_row(&buffer, "ROAD TRIP");
    assert!(chill < road_trip, "{}", view);
    assert!(pane_ends_with(&buffer, chill, "0"), "{}", view);
    assert!(pane_ends_with(&buffer, road_trip, "2"), "{}", view);
    assert!(
        view.contains("Empty. Press L on a track in tab 2 to add it."),
        "{}",
        view
    );
    assert!(view.contains("CHILL") && view.contains("0 TRACKS"), "{}", view);

    // Down selects Road Trip; the unknown duration is left out of the total and of its row.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Road Trip"));
    let buffer = common::tui::render(&mut app, 120, 30);
    let view = common::tui::rows(&buffer).join("\n");
    let label = common::tui::row(&buffer, row_with(&buffer, "2 TRACKS"));
    assert!(
        label.contains("ROAD TRIP") && label.contains("2 TRACKS  ·  04:44"),
        "{}",
        view
    );
    let airbag = common::tui::row(&buffer, row_with(&buffer, "AIRBAG"));
    assert!(airbag.contains("01   AIRBAG"), "{}", view);
    assert!(
        airbag.contains("RADIOHEAD") && airbag.trim_end().ends_with("04:44"),
        "{}",
        view
    );
    let paranoid = common::tui::row(&buffer, row_with(&buffer, "PARANOID ANDROID"));
    assert!(paranoid.contains("02   PARANOID ANDROID"), "{}", view);
    assert!(
        paranoid.trim_end().ends_with("PARANOID ANDROID"),
        "no length shown: {}",
        view
    );

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
    assert!(view.contains("1–5"), "{}", view);
    assert!(view.contains("PLAYLISTS (TAB 5)"), "{}", view);
    assert!(view.contains("Focus panes (tabs 2 & 5)"), "{}", view);
    assert!(view.contains("Open/Play / Queue playlist"), "{}", view);

    // The 20-row terminal the existing help test uses still renders without panicking.
    render(&mut app, 70, 20);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_fits_tab_5_keys_at_80x24() {
    common::isolate();
    let temp_dir = fresh_dirs("help80x24");
    let mut app = new_app(&temp_dir);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.show_help_modal);

    // The standard 80x24 terminal shows every line of both columns, unclipped.
    let view = render(&mut app, 80, 24);
    for text in [
        "PLAYLISTS (TAB 5)",
        "C ⇧R ⇧D",
        "↵ A",
        "New / Rename / Delete",
        "Open/Play / Queue playlist",
        "Focus panes (tabs 2 & 5)",
        "Page (2) / Move track (5)",
        "Tab: Now/Queue/Dev/Set/PL",
        "Full library fuzzy search",
    ] {
        assert!(view.contains(text), "80x24 help should show {:?}:\n{}", text, view);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

/// The tab row: the first row naming tab 5 (`5 PLAYLISTS`, or just `5` when narrow, so look for
/// the active tab's label).
fn tab_row(buffer: &ratatui::buffer::Buffer, active_label: &str) -> u16 {
    common::tui::find_row(buffer, active_label)
        .unwrap_or_else(|| panic!("{active_label:?} not drawn:\n{}", common::tui::rows(buffer).join("\n")))
}

#[test]
fn test_tab_row_shows_all_five_labels_at_80_columns() {
    common::isolate();
    let temp_dir = fresh_dirs("tabbar80");
    let mut app = new_app(&temp_dir);

    // Every tab selection renders every label at a standard 80-column width (the short set).
    for tab in 0..5 {
        app.active_tab = tab;
        let buffer = common::tui::render(&mut app, 80, 30);
        let row = common::tui::row(&buffer, tab_row(&buffer, "5 PLAYLISTS"));
        println!("tab {tab} row: {row}");
        for label in ["1 NOW", "2 QUEUE", "3 DEVICES", "4 SETTINGS", "5 PLAYLISTS"] {
            assert!(row.contains(label), "missing {label:?} in tab row (tab {tab}): {row:?}");
        }
    }

    // The label set follows the width: full from 110 columns, `QUEUE` from 90.
    let buffer = common::tui::render(&mut app, 120, 30);
    let row = common::tui::row(&buffer, tab_row(&buffer, "5 PLAYLISTS"));
    for label in [
        "1 NOW PLAYING",
        "2 QUEUE & LIBRARY",
        "3 DEVICES",
        "4 SETTINGS",
        "5 PLAYLISTS",
    ] {
        assert!(row.contains(label), "missing {label:?} at 120 columns: {row:?}");
    }
    let buffer = common::tui::render(&mut app, 100, 30);
    let row = common::tui::row(&buffer, tab_row(&buffer, "5 PLAYLISTS"));
    assert!(row.contains("1 NOW PLAYING") && row.contains("2 QUEUE    "), "{row:?}");
    assert!(!row.contains("LIBRARY"), "{row:?}");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// Render at `width` x 30 and check `label` sits whole on the tab row, drawn as the active tab
/// chip (the cursor style, padded by one cell on each side). Returns the tab row.
fn assert_active_tab_label(app: &mut AppUI, width: u16, label: &str) -> String {
    let buffer = common::tui::render(app, width, 30);
    let y = tab_row(&buffer, label);
    let row = common::tui::row(&buffer, y);
    let chip = format!(" {label} ");
    let (x, _) = common::tui::find_text(&buffer, &chip)
        .unwrap_or_else(|| panic!("{chip:?} not on the {width}-column tab row: {row:?}"));
    let cursor = quickstream::ui::theme::cursor();
    for cx in x..x + chip.chars().count() as u16 {
        let cell = &buffer[(cx, y)];
        assert!(cx < width, "{label:?} clipped at {width} columns: {row:?}");
        assert_eq!(
            cell.fg,
            cursor.fg.unwrap(),
            "column {cx} of {chip:?} at {width} columns: {row:?}"
        );
        assert_eq!(
            cell.bg,
            cursor.bg.unwrap(),
            "column {cx} of {chip:?} at {width} columns: {row:?}"
        );
        assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
    }
    // Only the active tab is a chip.
    let ink = cursor.bg.unwrap();
    let chip_cells = (0..width).filter(|&cx| buffer[(cx, y)].bg == ink).count();
    assert_eq!(chip_cells, chip.chars().count(), "one chip on the tab row: {row:?}");
    row
}

#[test]
fn test_tab_5_label_is_highlighted_at_80_columns_after_pressing_5() {
    common::isolate();
    let temp_dir = fresh_dirs("tabbar80hl");
    let mut app = new_app(&temp_dir);

    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.active_tab, 4);
    assert_active_tab_label(&mut app, 80, "5 PLAYLISTS");
    assert_active_tab_label(&mut app, 120, "5 PLAYLISTS");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_active_tab_label_stays_visible_below_79_columns() {
    common::isolate();
    // Issue #59: in a narrow terminal the active tab's label must stay whole and highlighted.
    let temp_dir = fresh_dirs("tabbar_narrow");
    let mut app = new_app(&temp_dir);
    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.active_tab, 4);

    // 79 and 78 columns still name every tab.
    for width in [79, 78] {
        let row = assert_active_tab_label(&mut app, width, "5 PLAYLISTS");
        for label in ["1 NOW", "2 QUEUE", "3 DEVICES", "4 SETTINGS"] {
            assert!(row.contains(label), "missing {label:?} at {width} columns: {row:?}");
        }
    }

    // Narrower, the other tabs show only their numbers.
    for width in [60, 40] {
        let row = assert_active_tab_label(&mut app, width, "5 PLAYLISTS");
        for label in [" 1 ", " 2 ", " 3 ", " 4 "] {
            assert!(row.contains(label), "missing {label:?} at {width} columns: {row:?}");
        }
        assert!(!row.contains("SETTINGS"), "{row:?}");
    }

    // Any tab can be the active one.
    app.active_tab = 1;
    assert_active_tab_label(&mut app, 79, "2 QUEUE");
    assert_active_tab_label(&mut app, 60, "2 QUEUE");
    assert_active_tab_label(&mut app, 40, "2 QUEUE");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// Column (in characters) where `text` starts on the first row of `view` containing it.
fn text_column(view: &str, text: &str) -> usize {
    let line = view
        .lines()
        .find(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text:?} not rendered:\n{view}"));
    line[..line.find(text).unwrap()].chars().count()
}

#[test]
fn test_help_modal_descriptions_line_up() {
    common::isolate();
    // Issue #59: `c / R / D` and `Enter/a` overflowed the key field, pushing their descriptions
    // out of line with the rest of the column.
    let temp_dir = fresh_dirs("help_align");
    let mut app = new_app(&temp_dir);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.show_help_modal);

    for (width, height) in [(80, 24), (120, 40)] {
        let view = render(&mut app, width, height);
        let left = [
            "Play / Pause",
            "Previous / Next track",
            "Tab: Now/Queue/Dev/Set/PL",
            "New / Rename / Delete",
            "Open/Play / Queue playlist",
        ];
        let right = [
            "Open / Queue / Play",
            "Autopilot (adds if playing)",
            "Add to playlist",
            "Set folder as startup home",
            "Full library fuzzy search",
        ];
        for column in [&left, &right] {
            let expected = text_column(&view, column[0]);
            for desc in column.iter() {
                assert_eq!(
                    text_column(&view, desc),
                    expected,
                    "{desc:?} is out of line at {width}x{height}:\n{view}"
                );
            }
        }
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_mix_picker_first_option_row_is_not_overdrawn() {
    common::isolate();
    // Issue #59: a leftover footer drew "Press [?], [Esc], or [q] to close" over the `[1]` row.
    let temp_dir = fresh_dirs("mix_picker");
    let mut app = new_app(&temp_dir);
    app.handle_input_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(app.show_mix_picker_modal);

    for (width, height) in [(80, 24), (120, 40)] {
        let view = render(&mut app, width, height);
        assert!(!view.contains("to close"), "{view}");
        // Skip the tab bar, whose first label also starts with `[1]`.
        let row = view
            .lines()
            .skip(1)
            .find(|l| l.contains("[1]"))
            .unwrap_or_else(|| panic!("no [1] row at {width}x{height}:\n{view}"));
        assert!(
            row.contains("[1] Diverse Artists Shuffle") && row.contains("— Fair random sampling"),
            "[1] row overdrawn at {width}x{height}: {row:?}"
        );
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

fn press_with(app: &mut AppUI, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap();
}

#[test]
fn test_playlists_tab_g_shift_g_and_page_keys_in_both_panes() {
    common::isolate();
    // Issue #52: g / G jump to the first / last row and PageUp / PageDown move a page, clamped
    // at the ends (no wraparound), in both panes.
    let temp_dir = fresh_dirs("jump_page");
    for i in 0..12 {
        write_playlist(&format!("List {:02}", i), Vec::new());
    }
    let tracks: Vec<PlaylistEntry> = (0..12)
        .map(|i| {
            entry(
                &temp_dir,
                &format!("t{:02}.flac", i),
                "A",
                &format!("Track {:02}", i),
                60,
            )
        })
        .collect();
    write_playlist("List 00", tracks);
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));
    app.playlists.list_page_height = 5;
    app.playlists.tracks_page_height = 4;

    // Left pane.
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.playlists.list_cursor, 5);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.playlists.list_cursor, 10);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.playlists.list_cursor, 11, "PageDown stops at the last row");
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.playlists.list_cursor, 11, "PageDown does not wrap");
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.playlists.list_cursor, 6);
    press(&mut app, KeyCode::PageUp);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.playlists.list_cursor, 0, "PageUp stops at the first row");
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.playlists.list_cursor, 0, "PageUp does not wrap");
    press_with(&mut app, KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!(app.playlists.list_cursor, 11);
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.playlists.list_cursor, 0);
    assert!(!app.playlists.tracks_focus);

    // Tracks pane: the playlist selection stays put.
    press(&mut app, KeyCode::Char('l'));
    assert!(app.playlists.tracks_focus);
    press_with(&mut app, KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!(app.playlists.track_cursor, 11);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.playlists.track_cursor, 11, "PageDown does not wrap");
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.playlists.track_cursor, 7);
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.playlists.track_cursor, 0);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.playlists.track_cursor, 4);
    assert_eq!(app.playlists.list_cursor, 0);

    // Home / End match g / G, as on tab 2.
    press(&mut app, KeyCode::End);
    assert_eq!(app.playlists.track_cursor, 11);
    press(&mut app, KeyCode::Home);
    assert_eq!(app.playlists.track_cursor, 0);

    // Moving to another playlist from the left pane starts its tracks at the top.
    press(&mut app, KeyCode::PageDown);
    press(&mut app, KeyCode::Char('h'));
    press_with(&mut app, KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!(app.playlists.list_cursor, 11);
    assert_eq!(app.playlists.track_cursor, 0);

    // None of these keys changed tab, playback or the queue.
    assert_eq!(app.active_tab, 4);
    assert!(app.queue.is_empty());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_page_size_follows_the_rendered_list_height() {
    common::isolate();
    let temp_dir = fresh_dirs("page_height");
    write_playlist("Alpha", vec![entry(&temp_dir, "a1.flac", "A", "One", 60)]);
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    render(&mut app, 120, 30);
    let (list_30, tracks_30) = (app.playlists.list_page_height, app.playlists.tracks_page_height);
    render(&mut app, 120, 40);
    let (list_40, tracks_40) = (app.playlists.list_page_height, app.playlists.tracks_page_height);
    assert!(list_30 > 1 && tracks_30 > 1, "{list_30} {tracks_30}");
    assert_eq!(list_40 - list_30, 10, "a page is the number of visible rows");
    assert_eq!(tracks_40 - tracks_30, 10, "a page is the number of visible rows");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_lists_page_keys() {
    common::isolate();
    let temp_dir = fresh_dirs("help_page");
    let mut app = new_app(&temp_dir);
    press(&mut app, KeyCode::Char('?'));
    for (width, height) in [(80, 24), (120, 40)] {
        let view = render(&mut app, width, height);
        for text in [
            "PgUp/PgDn",
            "Page Up / Down (tabs 2 & 5)",
            "Jump to Start / End",
            "~",
            "Jump to startup home dir",
        ] {
            assert!(
                view.contains(text),
                "{width}x{height} help should show {text:?}:\n{view}"
            );
        }
    }
    let _ = fs::remove_dir_all(&temp_dir);
}
