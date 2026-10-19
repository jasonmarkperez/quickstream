//! Short and small terminals (width rules 3h, last rule): under 24 rows the spacer rows go
//! first, the header folds into the tab row, Now Playing drops UP NEXT first, list tabs keep
//! their cursor row on screen and footnotes move to the label rows. Every tab is drawn at
//! 80x23, 80x20, 80x16, 120x20, 120x16, 40x12 and 20x8 with a full queue, a long folder, long
//! playlists and several renderers, and the chrome's guarantees are checked on each.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;

use common::fake_player::FakePlayer;
use common::tui;
use quickstream::discovery::DLNADevice;
use quickstream::metadata::AudioMetadata;
use quickstream::paths;
use quickstream::playlist::{Playlist, PlaylistEntry};
use quickstream::settings::SettingsManager;
use quickstream::ui::{theme, AppUI};

/// The sizes the short-terminal rules are audited at.
const SIZES: [(u16, u16); 7] = [(80, 23), (80, 20), (80, 16), (120, 20), (120, 16), (40, 12), (20, 8)];

/// Tracks in the folder, the queue and each playlist.
const TRACKS: usize = 30;

fn title(i: usize) -> String {
    format!("Track {:02} Halcyon Drift", i + 1)
}

fn file_name(i: usize) -> String {
    format!("{:02} Halcyon Drift.flac", i + 1)
}

fn press(app: &mut AppUI, code: KeyCode) {
    let _ = app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn renderer(i: usize) -> DLNADevice {
    let host = format!("192.168.1.{}", 10 + i);
    DLNADevice {
        friendly_name: format!("Renderer {:02}", i + 1),
        location: format!("http://{host}:8080/description.xml"),
        host: host.clone(),
        port: 8080,
        manufacturer: "Maker".to_string(),
        model_name: "Model".to_string(),
        av_transport_control_url: format!("http://{host}:8080/AVTransport/control"),
        rendering_control_url: format!("http://{host}:8080/RenderingControl/control"),
        udn: format!("uuid:renderer-{i}"),
    }
}

/// Write a playlist file straight into the playlists folder, as if saved by an earlier run.
fn write_playlist(name: &str, dir: &Path) {
    let playlists = paths::get_playlists_dir();
    fs::create_dir_all(&playlists).unwrap();
    let playlist = Playlist {
        name: name.to_string(),
        entries: (0..TRACKS)
            .map(|i| PlaylistEntry {
                path: dir.join(file_name(i)),
                duration_secs: 200 + i as i64,
                artist: "Mira Lenz".to_string(),
                title: title(i),
            })
            .collect(),
    };
    fs::write(playlists.join(format!("{name}.m3u8")), playlist.to_m3u8()).unwrap();
}

/// An app browsing a temp folder of [`TRACKS`] files and a few subfolders, with every file
/// queued (the first playing), two playlists of [`TRACKS`] entries each, eight renderers, and
/// every list's cursor on its last row (Up from the first row wraps there), so a list that
/// does not scroll to its cursor shows it off screen.
fn short_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_short_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for sub in ["Low Tide Atlas", "Sea Glass EP"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for i in 0..TRACKS {
        fs::write(dir.join(file_name(i)), b"").unwrap();
    }
    write_playlist("Road Trip", &dir);
    write_playlist("Night Drive", &dir);

    let mut settings = SettingsManager::with_path(dir.join("config.json"));
    settings.settings.metadata_view = false;
    let mut app = AppUI::new(Some(dir.clone()), settings);
    let start = Instant::now();
    while (app.browser_loading || !app.playlists.store.is_loaded()) && start.elapsed() < Duration::from_secs(5) {
        app.poll_background_dir_scan();
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.browser_loading, "folder listing did not finish");
    assert!(app.playlists.store.is_loaded(), "playlists did not load");
    // Every playlist file exists, so no row waits on a check.
    app.playlists
        .set_exists_checker(std::sync::Arc::new(|p: &Path| Some(p.to_path_buf())));

    for i in 0..TRACKS {
        app.queue.add_track(AudioMetadata {
            filepath: dir.join(file_name(i)),
            filename: file_name(i),
            title: title(i),
            artist: "Mira Lenz".to_string(),
            duration_seconds: 200.0 + i as f64,
            format_name: "flac".to_string(),
            sample_rate_hz: 96_000,
            ..Default::default()
        });
    }
    app.player = Box::new(FakePlayer {
        volume: 65,
        backend: Some("mpv"),
        ..FakePlayer::playing_at(61.0, 200.0)
    });
    *app.device_manager.devices.lock().unwrap() = (0..8).map(renderer).collect();

    // Cursors on the last rows: library, queue, devices, settings, playlists and tracks.
    app.active_tab = 1;
    app.queue_focus = false;
    press(&mut app, KeyCode::Up);
    app.queue_focus = true;
    press(&mut app, KeyCode::Up);
    app.active_tab = 2;
    press(&mut app, KeyCode::Up);
    app.active_tab = 3;
    press(&mut app, KeyCode::Up);
    app.active_tab = 4;
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Up);
    (app, dir)
}

/// The tab row: the first row with a reverse-video cell (the active tab's chip; the header, when
/// drawn, has none).
fn tab_row(buffer: &Buffer) -> u16 {
    let ink = theme::palette().ink;
    (0..buffer.area.height)
        .find(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == ink))
        .unwrap_or_else(|| panic!("no active tab chip\n{}", tui::rows(buffer).join("\n")))
}

/// The runs of reverse-video cells (INK background) on row `y`, as text.
fn chips(buffer: &Buffer, y: u16) -> Vec<String> {
    let ink = theme::palette().ink;
    let mut runs = Vec::new();
    let mut run: Option<String> = None;
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, y)];
        if cell.bg == ink {
            run.get_or_insert_with(String::new).push_str(cell.symbol());
        } else if let Some(text) = run.take() {
            runs.push(text);
        }
    }
    runs.extend(run);
    runs
}

/// The guarantees every frame keeps, whatever its size: every cell on GROUND, nothing in the
/// 2-column gutters, the active tab as a whole chip on the tab row, the key row last with its
/// keys whole, and the strip above it on tabs 2-5.
fn assert_chrome(buffer: &Buffer, tab: usize, what: &str) {
    let screen = tui::rows(buffer).join("\n");
    let (w, h) = (buffer.area.width, buffer.area.height);
    // GROUND, or the INK of a reverse-video chip or cursor, or the RULE of a keycap: never the
    // terminal's default background.
    let p = theme::palette();
    for y in 0..h {
        for x in 0..w {
            let bg = buffer[(x, y)].bg;
            assert!(
                [p.ground, p.ink, p.rule].contains(&bg),
                "{what}: cell ({x}, {y}) on {bg:?}\n{screen}"
            );
        }
        for x in [0, 1, w - 2, w - 1] {
            assert_eq!(
                buffer[(x, y)].symbol(),
                " ",
                "{what}: ({x}, {y}) in the gutter\n{screen}"
            );
        }
    }

    let tabs = tab_row(buffer);
    assert_eq!(tabs, if h < 24 { 0 } else { 3 }, "{what}: tab row\n{screen}");
    let active = chips(buffer, tabs);
    assert_eq!(active.len(), 1, "{what}: one active chip {active:?}\n{screen}");
    let chip = &active[0];
    assert!(
        chip.starts_with(&format!(" {} ", tab + 1)),
        "{what}: active chip {chip:?}\n{screen}"
    );
    assert!(
        chip.ends_with(' '),
        "{what}: chip padded on the right {chip:?}\n{screen}"
    );

    // The key row is the last row under 24 rows (else one above the bottom margin); every
    // keycap on it is followed by its whole label.
    let keys = if h < 24 { h - 1 } else { h - 2 };
    let key_text = tui::row(buffer, keys);
    let rule = theme::palette().rule;
    let caps: Vec<u16> = (0..w).filter(|&x| buffer[(x, keys)].bg == rule).collect();
    assert!(
        !caps.is_empty(),
        "{what}: no keycap on the key row {key_text:?}\n{screen}"
    );
    let last_cap = *caps.last().unwrap();
    let after: String = tui::cells(buffer, keys)[usize::from(last_cap) + 1..].concat();
    assert!(
        after.trim_start().chars().next().is_some_and(|c| c.is_alphabetic()),
        "{what}: last keycap without its label {key_text:?}\n{screen}"
    );

    if tab != 0 && h >= 3 {
        let strip = tui::row(buffer, keys - 1);
        assert!(
            strip.trim_start().starts_with("▶ TRACK"),
            "{what}: strip {strip:?}\n{screen}"
        );
    }
}

/// Rows other than the tab row with a cursor bar (reverse video), as text.
fn cursor_rows(buffer: &Buffer) -> Vec<String> {
    let tabs = tab_row(buffer);
    (0..buffer.area.height)
        .filter(|&y| y != tabs && !chips(buffer, y).is_empty())
        .map(|y| tui::row(buffer, y))
        .collect()
}

/// What the cursor row of the focused list on `tab` shows (the last row of each list), at
/// widths where the name column has room for it.
fn cursor_text(app: &AppUI, tab: usize) -> &'static str {
    match tab {
        // The 30th file of the folder, or the 30th track of the queue or the playlist.
        1 => "30",
        2 => "RENDERER 08",
        3 => "◂",
        4 if app.playlists.tracks_focus => "30",
        4 => "ROAD TRIP",
        _ => "",
    }
}

#[test]
fn every_tab_keeps_the_chrome_on_short_and_small_terminals() {
    common::isolate();
    let (mut app, dir) = short_app("chrome");
    for tab in 0..5 {
        app.active_tab = tab;
        for (w, h) in SIZES {
            let buffer = tui::render(&mut app, w, h);
            assert_chrome(&buffer, tab, &format!("tab {} at {w}x{h}", tab + 1));
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_list_keeps_its_cursor_row_on_screen() {
    common::isolate();
    let (mut app, dir) = short_app("cursor");
    // Tab 2 with either pane focused, tab 5 with either pane focused, devices and settings.
    let cases: [(usize, bool); 6] = [(1, true), (1, false), (2, false), (3, false), (4, true), (4, false)];
    for (tab, second_pane) in cases {
        app.active_tab = tab;
        app.queue_focus = second_pane;
        app.playlists.tracks_focus = second_pane;
        for (w, h) in SIZES {
            let buffer = tui::render(&mut app, w, h);
            let what = format!(
                "tab {} ({}) at {w}x{h}",
                tab + 1,
                if second_pane { "right" } else { "left" }
            );
            let cursors = cursor_rows(&buffer);
            assert_eq!(
                cursors.len(),
                1,
                "{what}: one cursor row {cursors:?}\n{}",
                tui::rows(&buffer).join("\n")
            );
            if w >= 40 {
                let wanted = cursor_text(&app, tab);
                assert!(
                    cursors[0].contains(wanted),
                    "{what}: cursor on {wanted:?}: {:?}\n{}",
                    cursors[0],
                    tui::rows(&buffer).join("\n")
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Too narrow for both of Now Playing's never-dropped keys: `N QUEUE` (#57) stays whole and
/// `? KEYS` gives way, rather than a `?` keycap without its label.
#[test]
fn a_key_row_too_narrow_for_its_top_keys_keeps_whole_ones() {
    common::isolate();
    let (mut app, dir) = short_app("narrow_keys");
    app.active_tab = 0;
    let buffer = tui::render(&mut app, 20, 8);
    let keys = tui::row(&buffer, 7);
    assert_eq!(keys.trim_end(), "   N  QUEUE", "{keys:?}");
    // Room for both again from 25 columns.
    let buffer = tui::render(&mut app, 25, 8);
    let keys = tui::row(&buffer, 7);
    assert!(keys.contains(" N  QUEUE") && keys.contains(" ?  KEYS"), "{keys:?}");
    let _ = fs::remove_dir_all(&dir);
}

/// The row under the first panel rule (`────`), as text.
fn row_under_rule(buffer: &Buffer) -> String {
    let rule = tui::find_row(buffer, "────").unwrap_or_else(|| panic!("no rule\n{}", tui::rows(buffer).join("\n")));
    tui::row(buffer, rule + 1)
}

/// Under 24 rows the blank row under a panel's rule is the first to go once a list does not
/// fit (3h): the lists start right under the rule. Taller terminals keep it.
#[test]
fn short_list_tabs_give_up_the_blank_row_under_the_rule() {
    common::isolate();
    let (mut app, dir) = short_app("spacer");
    for (tab, w, h) in [
        (1, 80, 20),
        (1, 120, 16),
        (2, 80, 12),
        (3, 80, 16),
        (4, 80, 20),
        (4, 40, 12),
    ] {
        app.active_tab = tab;
        let buffer = tui::render(&mut app, w, h);
        let under = row_under_rule(&buffer);
        assert!(
            !under.trim().is_empty(),
            "tab {} at {w}x{h}: list right under the rule\n{}",
            tab + 1,
            tui::rows(&buffer).join("\n")
        );
    }
    for tab in [1, 2, 3, 4] {
        app.active_tab = tab;
        let buffer = tui::render(&mut app, 80, 24);
        assert!(row_under_rule(&buffer).trim().is_empty(), "tab {} at 80x24", tab + 1);
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A short list that fits keeps its blank row: spacers go only to make room.
#[test]
fn a_short_list_that_fits_keeps_the_blank_row() {
    common::isolate();
    let (mut app, dir) = short_app("spacer_fits");
    app.active_tab = 2;
    *app.device_manager.devices.lock().unwrap() = vec![renderer(0)];
    app.devices_cursor = 0;
    let buffer = tui::render(&mut app, 80, 20);
    assert!(
        row_under_rule(&buffer).trim().is_empty(),
        "{}",
        tui::rows(&buffer).join("\n")
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Now Playing, too short for its spacer rows: they go before the elapsed row and the bar (UP
/// NEXT has gone already). With room for them they stay.
#[test]
fn now_playing_gives_up_its_spacer_rows_before_the_bar_and_elapsed_row() {
    common::isolate();
    let (mut app, dir) = short_app("np_spacers");
    app.active_tab = 0;
    let buffer = tui::render(&mut app, 40, 10);
    let screen = tui::rows(&buffer).join("\n");
    assert!(
        !row_under_rule(&buffer).trim().is_empty(),
        "numerals under the rule\n{screen}"
    );
    assert!(tui::find_row(&buffer, "▕").is_some(), "bar\n{screen}");
    assert!(screen.contains("ELAPSED 01:01"), "elapsed row\n{screen}");

    let buffer = tui::render(&mut app, 80, 16);
    assert!(
        row_under_rule(&buffer).trim().is_empty(),
        "{}",
        tui::rows(&buffer).join("\n")
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Too narrow for the big numerals and a title beside them: the time is written in plain
/// digits, whole, with the title under it, rather than numerals cut at the edge (`01:0`).
#[test]
fn now_playing_too_narrow_for_the_numerals_writes_the_time_plainly() {
    common::isolate();
    let (mut app, dir) = short_app("np_plain_time");
    app.active_tab = 0;
    let buffer = tui::render(&mut app, 20, 8);
    let screen = tui::rows(&buffer).join("\n");
    assert!(!screen.contains('┏'), "no numerals\n{screen}");
    assert!(screen.contains("01:01"), "the time\n{screen}");
    assert!(screen.contains("TRACK 01"), "the title\n{screen}");
    assert!(tui::find_row(&buffer, "▕").is_some(), "bar\n{screen}");
    // Wide enough for both: the numerals, with the title beside them.
    let buffer = tui::render(&mut app, 40, 12);
    let screen = tui::rows(&buffer).join("\n");
    assert!(screen.contains('┏') && screen.contains("TRACK 01"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

/// Every modal over every tab at the audited sizes: drawn without a panic, on GROUND, inside
/// the gutters (the help modal at 80-83 columns is the one exception, see test_ui_modals).
#[test]
fn every_modal_draws_on_short_and_small_terminals() {
    common::isolate();
    let (mut app, dir) = short_app("modals");
    for modal in 0..6 {
        for tab in 0..5 {
            app.active_tab = tab;
            match modal {
                0 => app.show_help_modal = true,
                1 => app.show_mix_picker_modal = true,
                2 => app.open_fuzzy_search(),
                3 => press_open(&mut app, 'c'),
                4 => press_open(&mut app, 'D'),
                _ => press_open(&mut app, 'L'),
            }
            for (w, h) in SIZES {
                let buffer = tui::render(&mut app, w, h);
                let what = format!("modal {modal} over tab {} at {w}x{h}", tab + 1);
                for y in 0..h {
                    for x in 0..w {
                        let cell = &buffer[(x, y)];
                        assert!(
                            cell.bg != ratatui::style::Color::Reset,
                            "{what}: ({x}, {y}) on the default background"
                        );
                    }
                    if modal != 0 || !(80..84).contains(&w) {
                        for x in [0, 1, w - 2, w - 1] {
                            assert_eq!(buffer[(x, y)].symbol(), " ", "{what}: ({x}, {y}) in the gutter");
                        }
                    }
                }
            }
            app.show_help_modal = false;
            app.show_mix_picker_modal = false;
            if let Some(mut search) = app.search_modal.take() {
                search.close();
            }
            app.playlists.modal = None;
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Open a playlist modal from tab 5's list pane with `key` (`c` name prompt, `D` delete
/// confirmation, `L` add picker), then go back to the tab the test is on.
fn press_open(app: &mut AppUI, key: char) {
    let tab = app.active_tab;
    let focus = app.playlists.tracks_focus;
    app.active_tab = 4;
    app.playlists.tracks_focus = false;
    press(app, KeyCode::Char(key));
    assert!(app.playlists.modal.is_some(), "{key} opens a modal");
    app.active_tab = tab;
    app.playlists.tracks_focus = focus;
}
