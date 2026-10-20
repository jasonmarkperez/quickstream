//! The Mono colour depth (`NO_COLOR`, issue #67): the screen draws no colour at all, and the cursor
//! bar, tab and modal chips, keycaps and the dimmed screen under a modal stay visible through
//! reverse video, bold and faint. Under `NO_COLOR` crossterm writes no colours, and the empty
//! colour command it writes instead (`ESC[;m`) resets the attributes too, so the frame must never
//! change colour from cell to cell. It lives in its own test binary because `theme::init` picks
//! the palette once per process.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Colored;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use common::fake_player::FakePlayer;
use common::tui;
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::search::SearchModalState;
use quickstream::ui::theme::{self, ColorDepth, Palette};
use quickstream::ui::AppUI;

const TITLES: [&str; 4] = ["Tidewater", "Glass Harbour", "Sodium Light", "Halcyon Drift"];

fn mono() {
    theme::init(ColorDepth::Mono);
    assert_eq!(theme::palette(), &Palette::MONO);
}

fn track(dir: &Path, i: usize) -> AudioMetadata {
    let filename = format!("{:02} {}.flac", i + 1, TITLES[i]);
    AudioMetadata {
        filepath: dir.join(&filename),
        filename,
        title: TITLES[i].to_string(),
        artist: "Mira Lenz".to_string(),
        album: "Low Tide Atlas".to_string(),
        genre: "Ambient".to_string(),
        format_name: "flac".to_string(),
        duration_seconds: 200.0 + 10.0 * i as f64,
        file_size: 48_200_000,
        bitrate_kbps: 2304,
        sample_rate_hz: 96_000,
        channels: 2,
    }
}

/// Browsing a folder of four tracks, all queued and the second playing.
fn mono_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_mono_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for i in 0..TITLES.len() {
        fs::write(&track(&dir, i).filepath, b"").unwrap();
    }
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    let start = Instant::now();
    while app.browser_loading {
        assert!(start.elapsed() < Duration::from_secs(5), "folder did not load");
        app.poll_background_dir_scan();
        std::thread::sleep(Duration::from_millis(5));
    }
    app.queue
        .add_tracks((0..TITLES.len()).map(|i| track(&dir, i)).collect());
    app.queue.set_current_index(1);
    app.player = Box::new(FakePlayer {
        volume: 65,
        backend: Some("mpv"),
        ..FakePlayer::playing_at(61.0, track(&dir, 1).duration_seconds)
    });
    app.active_tab = 1;
    (app, dir)
}

fn press(app: &mut AppUI, code: KeyCode) {
    let _ = app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn search(app: &mut AppUI, dir: &Path) {
    let tracks = (0..TITLES.len()).map(|i| track(dir, i)).collect();
    let mut search = SearchModalState::new(dir.to_path_buf(), tracks);
    search.update_query("ti".to_string());
    app.search_modal = Some(search);
}

/// Cells drawn in any colour other than the terminal's own.
fn coloured_cells(buffer: &Buffer) -> Vec<(u16, u16, Color, Color)> {
    let mut found = Vec::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.fg != Color::Reset || cell.bg != Color::Reset {
                found.push((x, y, cell.fg, cell.bg));
            }
        }
    }
    found
}

fn reversed(buffer: &Buffer, x: u16, y: u16) -> bool {
    buffer[(x, y)].modifier.contains(Modifier::REVERSED)
}

/// How many cells of row `y` are in reverse video.
fn reversed_cells(buffer: &Buffer, y: u16) -> usize {
    (0..buffer.area.width).filter(|&x| reversed(buffer, x, y)).count()
}

/// What the crossterm backend writes for `buffer` drawn over an empty screen.
fn terminal_output(buffer: &Buffer) -> String {
    let mut out = Vec::new();
    {
        let mut backend = CrosstermBackend::new(&mut out);
        let empty = Buffer::empty(buffer.area);
        backend.draw(empty.diff(buffer).into_iter()).unwrap();
    }
    String::from_utf8(out).unwrap()
}

/// The output before the reset ratatui writes once the frame is drawn (`ESC[0m`, after the
/// colour resets that are empty under `NO_COLOR`).
fn frame_body(out: &str) -> &str {
    let end = out.rfind("\x1b[0m").expect("the closing attribute reset");
    out[..end].trim_end_matches("\x1b[m")
}

/// Whether `body` holds an SGR command that resets the attributes: `ESC[m`, `ESC[0m`, or the
/// empty colour pair `ESC[;m` crossterm writes under `NO_COLOR`.
fn resets_attributes(body: &str) -> bool {
    ["\x1b[m", "\x1b[0m", "\x1b[;m"].iter().any(|r| body.contains(r))
}

#[test]
fn every_tab_and_modal_draws_without_colour() {
    common::isolate();
    mono();
    let (mut app, dir) = mono_app("colour");
    for (w, h) in [(120, 40), (80, 24), (60, 16)] {
        for tab in 0..5 {
            app.active_tab = tab;
            let buffer = tui::render(&mut app, w, h);
            assert_eq!(coloured_cells(&buffer), vec![], "tab {} at {w}x{h}", tab + 1);
        }
        app.active_tab = 1;
        press(&mut app, KeyCode::Char('?'));
        assert!(app.show_help_modal);
        let buffer = tui::render(&mut app, w, h);
        assert_eq!(coloured_cells(&buffer), vec![], "help at {w}x{h}");
        app.show_help_modal = false;

        search(&mut app, &dir);
        let buffer = tui::render(&mut app, w, h);
        assert_eq!(coloured_cells(&buffer), vec![], "search at {w}x{h}");
        app.search_modal.take().unwrap().close();

        app.active_tab = 0;
        press(&mut app, KeyCode::Char('M'));
        assert!(app.show_mix_picker_modal);
        let buffer = tui::render(&mut app, w, h);
        assert_eq!(coloured_cells(&buffer), vec![], "mix picker at {w}x{h}");
        app.show_mix_picker_modal = false;
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cursor_chips_and_keycaps_show_in_reverse_video() {
    common::isolate();
    mono();
    let (mut app, dir) = mono_app("marks");
    app.queue_focus = false;
    let buffer = tui::render(&mut app, 120, 40);

    // The active tab chip.
    let styles = tui::text_styles(&buffer, " 2 QUEUE & LIBRARY ").expect("tab chip");
    assert!(styles
        .iter()
        .all(|s| s.add_modifier.contains(Modifier::REVERSED | Modifier::BOLD)));
    let (_, tab_row) = tui::find_text(&buffer, " 2 QUEUE & LIBRARY ").unwrap();
    // The other tabs are not.
    assert!(tui::text_styles(&buffer, "3 DEVICES")
        .unwrap()
        .iter()
        .all(|s| !s.add_modifier.contains(Modifier::REVERSED)));

    // A key row keycap, and not its label.
    let (kx, ky) = tui::find_text(&buffer, " ?  KEYS").expect("? KEYS");
    assert!((kx..kx + 3).all(|x| reversed(&buffer, x, ky)), "keycap ` ? `");
    assert!((kx + 4..kx + 8).all(|x| !reversed(&buffer, x, ky)), "label KEYS");

    // The library's cursor bar: a row in reverse video across its pane.
    let bars: Vec<u16> = (0..buffer.area.height)
        .filter(|&y| y != tab_row && reversed_cells(&buffer, y) >= 40)
        .collect();
    assert_eq!(bars.len(), 1, "one cursor bar, rows {bars:?}");

    // Under a modal the screen is faint, the modal's chip in reverse video.
    press(&mut app, KeyCode::Char('?'));
    let buffer = tui::render(&mut app, 120, 40);
    let header = tui::text_styles(&buffer, "QUICKSTREAM").expect("header");
    assert!(
        header.iter().all(|s| s.add_modifier.contains(Modifier::DIM)),
        "{header:?}"
    );
    let chip = tui::text_styles(&buffer, " KEYS ").expect("help chip");
    assert!(
        chip.iter().all(|s| s.add_modifier.contains(Modifier::REVERSED)),
        "{chip:?}"
    );
    app.show_help_modal = false;

    // Search's block cursor after the query.
    search(&mut app, &dir);
    let buffer = tui::render(&mut app, 120, 40);
    let (qx, qy) = tui::find_text(&buffer, "QUERY  ti").expect("query row");
    assert!(reversed(&buffer, qx + 9, qy), "block cursor");
    let _ = fs::remove_dir_all(&dir);
}

/// The cells of row `y` that show (text, or reverse video) and are faint.
fn faint_cells(buffer: &Buffer, y: u16) -> Vec<(u16, String)> {
    (0..buffer.area.width)
        .filter(|&x| {
            let cell = &buffer[(x, y)];
            (cell.symbol() != " " || cell.modifier.contains(Modifier::REVERSED))
                && cell.modifier.contains(Modifier::DIM)
        })
        .map(|x| (x, buffer[(x, y)].symbol().to_string()))
        .collect()
}

#[test]
fn key_row_under_a_modal_is_not_faint() {
    common::isolate();
    mono();
    let (mut app, dir) = mono_app("keyrow");
    // The key row is drawn after the screen under the modal is made faint: its keycaps, labels
    // and status read as they do without a modal.
    for modal in ["help", "search"] {
        app.status_message = "Added to queue: Tidewater".to_string();
        if modal == "help" {
            press(&mut app, KeyCode::Char('?'));
            assert!(app.show_help_modal);
        } else {
            search(&mut app, &dir);
        }
        let buffer = tui::render(&mut app, 120, 40);
        let y = buffer.area.height - 2;
        let row = tui::row(&buffer, y);
        assert!(row.contains("Added to queue: Tidewater"), "{modal}: {row:?}");
        assert!(row.contains("ESC"), "{modal}: {row:?}");
        assert_eq!(faint_cells(&buffer, y), vec![], "{modal}: {row:?}");
        // The keycaps are still in reverse video.
        assert!(reversed_cells(&buffer, y) >= 5, "{modal}: no keycaps");
        // While the screen above it is faint.
        let header = tui::text_styles(&buffer, "QUICKSTREAM").expect("header");
        assert!(header.iter().all(|s| s.add_modifier.contains(Modifier::DIM)), "{modal}");
        app.show_help_modal = false;
        if let Some(mut search) = app.search_modal.take() {
            search.close();
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn spotlight_footnote_keycap_and_query_are_not_faint() {
    common::isolate();
    mono();
    let (mut app, dir) = mono_app("spotlight");
    app.queue_focus = false;
    app.open_spotlight();
    app.browser_spotlight = Some("zq".to_string());
    let buffer = tui::render(&mut app, 120, 40);
    // The footnote is drawn over DIM; its keycap, query, block cursor and count are not faint.
    let (x, y) = tui::find_text(&buffer, " /   zq ").expect("spotlight footnote");
    let styles = tui::text_styles(&buffer, " /   zq ").unwrap();
    for (i, s) in styles.iter().enumerate() {
        let blank = i == 3 || i == 4;
        assert!(blank || !s.add_modifier.contains(Modifier::DIM), "cell {i}: {s:?}");
    }
    assert!((x..x + 3).all(|x| reversed(&buffer, x, y)), "keycap ` / `");
    assert!(styles[5].add_modifier.contains(Modifier::BOLD), "query in bold");
    assert!(reversed(&buffer, x + 7, y), "block cursor");
    let count = tui::text_styles(&buffer, "NO MATCHING SUBFOLDERS").expect("count");
    assert!(
        count.iter().all(|s| !s.add_modifier.contains(Modifier::DIM)),
        "{count:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_color_output_keeps_the_attributes() {
    common::isolate();
    mono();
    // As crossterm does when NO_COLOR is set.
    Colored::set_ansi_color_disabled(true);

    // The check sees the failure it guards against: a coloured cursor writes an empty colour
    // command after its bold, which resets it.
    let mut coloured = Buffer::empty(Rect::new(0, 0, 4, 1));
    coloured.set_string(0, 0, " 2 ", Palette::TRUECOLOR.cursor());
    assert!(resets_attributes(frame_body(&terminal_output(&coloured))));

    let (mut app, dir) = mono_app("output");
    for tab in 0..5 {
        app.active_tab = tab;
        let buffer = tui::render(&mut app, 120, 40);
        let out = terminal_output(&buffer);
        let body = frame_body(&out);
        assert!(
            !resets_attributes(body),
            "tab {} resets its attributes mid-frame",
            tab + 1
        );
        assert!(body.contains("\x1b[7m"), "tab {}: no reverse video written", tab + 1);
    }
    Colored::set_ansi_color_disabled(false);
    let _ = fs::remove_dir_all(&dir);
}
