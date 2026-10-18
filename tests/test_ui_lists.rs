//! Tab 2 Queue & Library in the Telemetry style (mockups 2c, 3a, 3g): the LIBRARY and QUEUE
//! panels, their columns, the IN QUEUE / `▪` markers, the cursor, the spotlight, the folder
//! duration and the width rules.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use common::tui;
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::theme::Palette;
use quickstream::ui::AppUI;

const P: Palette = Palette::TRUECOLOR;

/// Library panel text starts here (2-column gutter); at 120 columns the text area is 54 columns
/// (56-column panel minus the ` ▪`), and the queue panel starts 4 columns after the library's.
const LIB_X: u16 = 2;
const TEXT_W: u16 = 54;
const QUEUE_X: u16 = 62;

/// Files in every test folder, with the tags the tests put in the metadata cache.
const FILES: [(&str, &str, &str, f64); 3] = [
    ("01 Tidewater.flac", "Tidewater", "Mira Lenz", 242.0),
    ("02 Glass Harbour.flac", "Glass Harbour", "Mira Lenz", 258.0),
    ("03 Halcyon Drift.mp3", "Halcyon Drift", "Oren Vasquez", 352.0),
];

fn meta(dir: &Path, file: &str, title: &str, artist: &str, secs: f64) -> AudioMetadata {
    AudioMetadata {
        filepath: dir.join(file),
        filename: file.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        duration_seconds: secs,
        format_name: Path::new(file)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// A temp folder with three subfolders and the three [`FILES`] (empty files: only the names
/// matter), and an app browsing it on tab 2. `tagged` files get their tags in the metadata cache;
/// the app is in Tags mode. The listing is complete when this returns.
fn lists_app(tag: &str, tagged: &[usize]) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_lists_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for sub in ["Low Tide Atlas", "Sea Glass EP", "Slow Low Summer"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for (file, ..) in FILES {
        fs::write(dir.join(file), b"").unwrap();
    }
    let mut settings = SettingsManager::with_path(dir.join("config.json"));
    // Files mode while the folder is listed, so no background tag read races the tags below.
    settings.settings.metadata_view = false;
    let mut app = AppUI::new(Some(dir.clone()), settings);
    wait_for_listing(&mut app);
    for &i in tagged {
        let (file, title, artist, secs) = FILES[i];
        app.metadata_cache.insert(meta(&dir, file, title, artist, secs));
    }
    app.settings_mgr.settings.metadata_view = true;
    app.update_browser_entry_labels();
    app.active_tab = 1;
    (app, dir)
}

fn wait_for_listing(app: &mut AppUI) {
    let start = Instant::now();
    while app.browser_loading && start.elapsed() < Duration::from_secs(5) {
        app.poll_background_dir_scan();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.browser_loading, "folder listing did not finish");
}

fn queue_track(dir: &Path, i: usize) -> AudioMetadata {
    let (file, title, artist, secs) = FILES[i];
    meta(dir, file, title, artist, secs)
}

/// Position of `text` with its first cell at or after column `from` and before column `to`.
fn find_in(buf: &Buffer, text: &str, from: u16, to: u16) -> Option<(u16, u16)> {
    let wanted: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    for y in 0..buf.area.height {
        let row = tui::cells(buf, y);
        for x in from..to {
            let x = x as usize;
            if x + wanted.len() <= row.len() && row[x..x + wanted.len()] == wanted[..] {
                return Some((x as u16, y));
            }
        }
    }
    None
}

fn lib(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
    find_in(buf, text, 0, QUEUE_X - 2)
}

fn queue(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
    find_in(buf, text, QUEUE_X - 2, buf.area.width)
}

/// The text of row `y` between columns `from` and `to`.
fn span_text(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    tui::cells(buf, y)[from as usize..to as usize].concat()
}

fn screen(buf: &Buffer) -> String {
    tui::rows(buf).join("\n")
}

// ---------------------------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------------------------

#[test]
fn panels_and_columns_at_120_cols() {
    common::isolate();
    let (mut app, dir) = lists_app("cols", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 2));
    app.queue.add_track(queue_track(&dir, 1));
    app.queue.set_current_index(1);
    app.queue_cursor = 1;
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);

    // Panel label rows: LIBRARY with TAGS, QUEUE with the queue mode, on one row.
    // (The tab row also says LIBRARY; the panel label is the one at the gutter.)
    let (_, y) = find_in(&buf, "LIBRARY", LIB_X, LIB_X + 1).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(lib(&buf, "TAGS"), Some((LIB_X + TEXT_W - 4, y)), "{shown}");
    assert_eq!(
        queue(&buf, "QUEUE").map(|(qx, qy)| (qx, qy == y)),
        Some((QUEUE_X, true)),
        "{shown}"
    );
    assert_eq!(queue(&buf, "LOOP"), Some((QUEUE_X + TEXT_W - 4, y)), "{shown}");

    // Folder rows: `DIR`, then the name in UPPERCASE with `/`, no child count.
    let (dx, dy) = lib(&buf, "LOW TIDE ATLAS/").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(dx, LIB_X + 6);
    assert_eq!(span_text(&buf, dy, LIB_X, LIB_X + 6), "DIR   ");
    assert_eq!(
        span_text(&buf, dy, dx + 15, LIB_X + TEXT_W).trim(),
        "",
        "no child count"
    );
    assert!(!tui::cell_style(&buf, dx, dy).add_modifier.contains(Modifier::REVERSED));
    assert_eq!(tui::cell_style(&buf, LIB_X, dy).fg, Some(P.dim), "DIR is DIM");
    assert!(
        tui::cell_style(&buf, dx, dy).add_modifier.contains(Modifier::BOLD),
        "folder names are bold"
    );

    // Music Homes and parent rows.
    let (_, hy) = lib(&buf, "MUSIC HOMES").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, hy, LIB_X, LIB_X + 1), "★");
    let (_, py) = lib(&buf, "PARENT").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, py, LIB_X, LIB_X + 2), "..");

    // File rows: number, TITLE, ARTIST, format and duration against the right edge.
    // Tags mode sorts by artist and title: GLASS HARBOUR, TIDEWATER, then HALCYON DRIFT.
    let (tx, ty) = lib(&buf, "TIDEWATER").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(tx, LIB_X + 6, "{shown}");
    assert_eq!(span_text(&buf, ty, LIB_X, LIB_X + 2), "02");
    assert_eq!(tui::cell_style(&buf, LIB_X, ty).fg, Some(P.dim), "number is DIM");
    assert_eq!(tui::cell_style(&buf, tx, ty).fg, Some(P.ink), "title is INK");
    let ax = (tx + 9..QUEUE_X - 2)
        .find(|&x| span_text(&buf, ty, x, x + 9) == "MIRA LENZ")
        .unwrap_or_else(|| panic!("artist on the title's row: {shown}"));
    assert_eq!(tui::cell_style(&buf, ax, ty).fg, Some(P.mid), "artist is MID");
    assert_eq!(
        lib(&buf, "FLAC  04:02"),
        Some((LIB_X + TEXT_W - 11, ty)),
        "format and duration end at the text edge: {shown}"
    );
    assert_eq!(tui::cell_style(&buf, LIB_X + TEXT_W - 1, ty).fg, Some(P.mid));

    // Queue rows: marker, number, TITLE, ARTIST, duration against the right edge.
    let (qx, qy) = queue(&buf, "HALCYON DRIFT").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(qx, QUEUE_X + 6);
    assert_eq!(span_text(&buf, qy, QUEUE_X, QUEUE_X + 6), "  01  ");
    assert_eq!(
        tui::cell_style(&buf, QUEUE_X + 2, qy).fg,
        Some(P.dim),
        "queue number is DIM"
    );
    let qax = (qx + 13..108)
        .find(|&x| span_text(&buf, qy, x, x + 12) == "OREN VASQUEZ")
        .unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(tui::cell_style(&buf, qax, qy).fg, Some(P.mid));
    assert_eq!(queue(&buf, "05:52"), Some((QUEUE_X + TEXT_W - 5, qy)), "{shown}");

    // Footnotes: the folder path keeps its case; the queue counts tracks and time.
    let dir_text = dir.to_string_lossy().to_string();
    let tail: String = dir_text
        .chars()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert!(shown.contains(&tail), "path in original case: {shown}");
    assert!(shown.contains("2 TRACKS  ·  10:10"), "{shown}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn page_heights_are_the_rows_each_list_shows() {
    common::isolate();
    let (mut app, dir) = lists_app("page", &[0, 1, 2]);
    for i in 0..40 {
        app.queue.add_track(queue_track(&dir, i % 3));
    }
    for (w, h) in [(120, 40), (120, 30), (100, 30), (85, 30)] {
        let buf = tui::render(&mut app, w, h);
        // The queue shows its first page: count the numbered rows.
        let numbers = (0..h).filter(|&y| {
            let row = tui::row(&buf, y);
            (1..=40).any(|n| {
                ["TIDEWATER", "GLASS HARBOUR", "HALCYON DRIFT"]
                    .iter()
                    .any(|title| row.contains(&format!(" {n:02}  {title}")))
            })
        });
        assert_eq!(numbers.count(), app.queue_page_height, "{w}x{h}:\n{}", screen(&buf));
        assert_eq!(app.browser_page_height, app.queue_page_height, "{w}x{h}");
    }
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// IN QUEUE and the playing marker
// ---------------------------------------------------------------------------------------------

#[test]
fn in_queue_marker_follows_the_queue() {
    common::isolate();
    let (mut app, dir) = lists_app("inqueue", &[0, 1, 2]);
    let buf = tui::render(&mut app, 120, 40);
    assert!(lib(&buf, "IN QUEUE").is_none(), "{}", screen(&buf));

    app.queue.add_track(queue_track(&dir, 0));
    app.queue.add_track(queue_track(&dir, 1));
    app.queue.set_current_index(1);
    let buf = tui::render(&mut app, 120, 40);
    let (_, ty) = lib(&buf, "TIDEWATER").unwrap();
    assert_eq!(
        lib(&buf, "IN QUEUE  04:02"),
        Some((LIB_X + TEXT_W - 15, ty)),
        "{}",
        screen(&buf)
    );
    let (_, gy) = lib(&buf, "GLASS HARBOUR").unwrap();
    assert!(span_text(&buf, gy, LIB_X, LIB_X + TEXT_W).contains("IN QUEUE  04:18"));
    let (_, hy) = lib(&buf, "HALCYON DRIFT").unwrap();
    assert!(span_text(&buf, hy, LIB_X, LIB_X + TEXT_W).contains("MP3  05:52"));

    app.queue.remove_track(0);
    let buf = tui::render(&mut app, 120, 40);
    let (_, ty) = lib(&buf, "TIDEWATER").unwrap();
    assert!(
        span_text(&buf, ty, LIB_X, LIB_X + TEXT_W).contains("FLAC  04:02"),
        "{}",
        screen(&buf)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playing_track_gets_the_square_in_the_library_and_the_arrow_in_the_queue() {
    common::isolate();
    let (mut app, dir) = lists_app("playing", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 0));
    app.queue.add_track(queue_track(&dir, 1));
    app.queue.set_current_index(1);
    let buf = tui::render(&mut app, 120, 40);

    let (_, gy) = lib(&buf, "GLASS HARBOUR").unwrap();
    assert_eq!(span_text(&buf, gy, LIB_X, LIB_X + 2), "▪ ", "{}", screen(&buf));
    assert_eq!(tui::cell_style(&buf, LIB_X, gy).fg, Some(P.ink));
    let (_, ty) = lib(&buf, "TIDEWATER").unwrap();
    assert_eq!(span_text(&buf, ty, LIB_X, LIB_X + 2), "02", "files keep their numbers");

    let (_, qy) = queue(&buf, "GLASS HARBOUR").unwrap();
    assert_eq!(span_text(&buf, qy, QUEUE_X, QUEUE_X + 4), "▶ 02");
    for x in [QUEUE_X + 2, QUEUE_X + 6] {
        let style = tui::cell_style(&buf, x, qy);
        assert_eq!(style.fg, Some(P.ink), "playing row is INK");
        assert!(style.add_modifier.contains(Modifier::BOLD), "playing row is bold");
    }
    let (_, oy) = queue(&buf, "TIDEWATER").unwrap();
    assert!(!tui::cell_style(&buf, QUEUE_X + 6, oy)
        .add_modifier
        .contains(Modifier::BOLD));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn in_queue_set_is_rebuilt_only_when_the_queue_changes() {
    common::isolate();
    let (mut app, dir) = lists_app("rebuild", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 0));
    tui::render(&mut app, 120, 40);
    let builds = app.in_queue_set_builds();
    for _ in 0..5 {
        tui::render(&mut app, 120, 40);
    }
    assert_eq!(
        app.in_queue_set_builds(),
        builds,
        "no rebuild while the queue is unchanged"
    );

    app.queue.add_track(queue_track(&dir, 1));
    tui::render(&mut app, 120, 40);
    tui::render(&mut app, 120, 40);
    assert_eq!(app.in_queue_set_builds(), builds + 1, "one rebuild per queue change");
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Cursor and focus
// ---------------------------------------------------------------------------------------------

#[test]
fn cursor_bar_in_the_focused_pane_and_marker_in_the_other() {
    common::isolate();
    let (mut app, dir) = lists_app("cursor", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 0));
    app.queue.add_track(queue_track(&dir, 1));
    app.queue_cursor = 1;
    let folder = app
        .browser_entries
        .iter()
        .position(|(_, p, _)| p.ends_with("Sea Glass EP"))
        .unwrap();
    app.browser_cursor = folder;
    let buf = tui::render(&mut app, 120, 40);

    // Library focused: the cursor bar runs one column past the text on each side.
    let (_, y) = lib(&buf, "SEA GLASS EP/").unwrap();
    for x in LIB_X - 1..=LIB_X + TEXT_W {
        let style = tui::cell_style(&buf, x, y);
        assert_eq!((style.fg, style.bg), (Some(P.ground), Some(P.ink)), "x={x}");
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }
    assert_eq!(tui::cell_style(&buf, LIB_X - 2, y).bg, Some(P.ground));
    assert_eq!(tui::cell_style(&buf, LIB_X + TEXT_W + 1, y).bg, Some(P.ground));
    // Queue unfocused: `▸` in its marker column, no bar.
    let (_, qy) = queue(&buf, "GLASS HARBOUR").unwrap();
    assert_eq!(span_text(&buf, qy, QUEUE_X, QUEUE_X + 1), "▸");
    assert_eq!(tui::cell_style(&buf, QUEUE_X, qy).fg, Some(P.mid));
    assert_ne!(tui::cell_style(&buf, QUEUE_X + 6, qy).bg, Some(P.ink));

    // `l` moves the focus: now the queue has the bar and the library the marker.
    app.handle_tab_key(KeyCode::Char('l'));
    let buf = tui::render(&mut app, 120, 40);
    let (_, qy) = queue(&buf, "GLASS HARBOUR").unwrap();
    for x in QUEUE_X - 1..=QUEUE_X + TEXT_W {
        assert_eq!(tui::cell_style(&buf, x, qy).bg, Some(P.ink), "x={x}");
    }
    let (_, y) = lib(&buf, "SEA GLASS EP/").unwrap();
    assert_eq!(span_text(&buf, y, LIB_X, LIB_X + 1), "▸");
    assert_ne!(tui::cell_style(&buf, LIB_X + 6, y).bg, Some(P.ink));
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Width rules
// ---------------------------------------------------------------------------------------------

#[test]
fn at_85_cols_the_artist_and_format_columns_drop_and_panels_are_compact() {
    common::isolate();
    let (mut app, dir) = lists_app("w85", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 0));
    app.queue.add_track(queue_track(&dir, 2));
    let buf = tui::render(&mut app, 85, 30);
    let shown = screen(&buf);

    assert!(!shown.contains("MIRA LENZ"), "no artist column: {shown}");
    assert!(!shown.contains("OREN VASQUEZ"), "no artist column: {shown}");
    assert!(!shown.contains("FLAC"), "no format column: {shown}");
    let (_, ty) = tui::find_text(&buf, "TIDEWATER").unwrap();
    assert!(tui::row(&buf, ty).contains("04:02"), "{shown}");

    // Compact panels: the position sits on the label row; no footnote row with the path.
    let (_, ly) = tui::find_text(&buf, "LIBRARY").unwrap();
    let label_row = tui::row(&buf, ly);
    let pos = format!("{}/{}", app.browser_cursor + 1, app.browser_entries.len());
    assert!(label_row.contains(&pos), "{label_row:?}");
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(!shown.contains(&name), "no path footnote when compact: {shown}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn under_80_cols_only_the_focused_pane_shows_and_l_switches() {
    common::isolate();
    let (mut app, dir) = lists_app("w70", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 2));
    let starts_with = |buf: &Buffer, text: &str| {
        (0..buf.area.height).any(|y| span_text(buf, y, LIB_X, buf.area.width).starts_with(text))
    };

    let buf = tui::render(&mut app, 70, 24);
    assert!(starts_with(&buf, "LIBRARY"), "{}", screen(&buf));
    assert!(!starts_with(&buf, "QUEUE"), "{}", screen(&buf));
    assert!(tui::find_text(&buf, "LOW TIDE ATLAS/").is_some());

    app.handle_tab_key(KeyCode::Char('l'));
    let buf = tui::render(&mut app, 70, 24);
    assert!(starts_with(&buf, "QUEUE"), "{}", screen(&buf));
    assert!(!starts_with(&buf, "LIBRARY"), "{}", screen(&buf));
    assert!(tui::find_text(&buf, "HALCYON DRIFT").is_some());
    assert!(tui::find_text(&buf, "LOW TIDE ATLAS/").is_none());

    app.handle_tab_key(KeyCode::Char('h'));
    let buf = tui::render(&mut app, 70, 24);
    assert!(starts_with(&buf, "LIBRARY"), "{}", screen(&buf));
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Spotlight
// ---------------------------------------------------------------------------------------------

#[test]
fn spotlight_underlines_matches_and_dims_the_rest() {
    common::isolate();
    let (mut app, dir) = lists_app("spot", &[0, 1, 2]);
    app.open_spotlight();
    app.browser_spotlight = Some("low".to_string());
    app.jump_spotlight_to_first_match();
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);

    // Footnote row: the `/` keycap, the query as typed and the match count.
    let (kx, ky) = lib(&buf, " / ").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(tui::cell_style(&buf, kx, ky).bg, Some(P.rule), "keycap");
    let (qx, qy) = lib(&buf, "low").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(qy, ky);
    assert!(tui::cell_style(&buf, qx, qy).add_modifier.contains(Modifier::BOLD));
    assert!(
        span_text(&buf, ky, LIB_X, LIB_X + TEXT_W).contains("1 OF 2 SUBFOLDERS"),
        "{shown}"
    );
    assert!(lib(&buf, "SPOTLIGHT").is_some(), "label note: {shown}");

    // Matches: the matched characters are underlined, the rest of the name is not.
    let (x, y) = lib(&buf, "LOW TIDE ATLAS/").unwrap();
    for i in 0..3 {
        assert!(tui::cell_style(&buf, x + i, y)
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }
    assert!(!tui::cell_style(&buf, x + 4, y)
        .add_modifier
        .contains(Modifier::UNDERLINED));
    let (x, y) = lib(&buf, "SLOW LOW SUMMER/").unwrap();
    assert!(!tui::cell_style(&buf, x, y).add_modifier.contains(Modifier::UNDERLINED));
    for i in 1..4 {
        assert!(tui::cell_style(&buf, x + i, y)
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }
    assert_ne!(
        tui::cell_style(&buf, x, y).fg,
        Some(P.dim),
        "matching rows keep their colours"
    );

    // Everything else drops to DIM.
    for text in ["SEA GLASS EP/", "TIDEWATER", "MUSIC HOMES", "PARENT"] {
        let (x, y) = lib(&buf, text).unwrap_or_else(|| panic!("{text}: {shown}"));
        assert_eq!(tui::cell_style(&buf, x, y).fg, Some(P.dim), "{text} is DIM");
    }

    // No match: the count turns ERROR and the list is left as it is.
    app.browser_spotlight = Some("lox".to_string());
    let buf = tui::render(&mut app, 120, 40);
    let (x, y) = lib(&buf, "NO MATCHING SUBFOLDERS").unwrap_or_else(|| panic!("{}", screen(&buf)));
    assert_eq!(tui::cell_style(&buf, x, y).fg, Some(P.error));
    let (x, y) = lib(&buf, "SEA GLASS EP/").unwrap();
    assert_ne!(tui::cell_style(&buf, x, y).fg, Some(P.dim));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn spotlight_keeps_its_footnote_row_on_compact_screens() {
    common::isolate();
    let (mut app, dir) = lists_app("spot80", &[0, 1, 2]);
    app.open_spotlight();
    app.browser_spotlight = Some("sea".to_string());
    let buf = tui::render(&mut app, 80, 24);
    let shown = screen(&buf);
    assert!(tui::find_text(&buf, "sea").is_some(), "{shown}");
    assert!(
        shown.contains("1 OF 1 SUBFOLDERS") || shown.contains("1 OF 1 SUBFOLDER"),
        "{shown}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Wide characters
// ---------------------------------------------------------------------------------------------

#[test]
fn cjk_titles_keep_the_columns_aligned() {
    common::isolate();
    let (mut app, dir) = lists_app("cjk", &[0, 1, 2]);
    let mut wide = queue_track(&dir, 0);
    wide.filepath = dir.join("夜.flac");
    wide.title = "東京の夜に降る雨のメロディーと長い長いタイトル".to_string();
    wide.artist = "坂本".to_string();
    app.queue.add_track(wide);
    app.queue.add_track(queue_track(&dir, 2));
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);

    let (_, wy) = queue(&buf, "東").unwrap_or_else(|| panic!("{shown}"));
    let (_, ay) = queue(&buf, "HALCYON DRIFT").unwrap();
    let dur_x = |y: u16| {
        (QUEUE_X..120)
            .find(|&x| span_text(&buf, y, x, x + 5).chars().nth(2) == Some(':'))
            .unwrap_or_else(|| panic!("no duration on row {y}: {shown}"))
    };
    assert_eq!(dur_x(wy), dur_x(ay), "durations line up: {shown}");
    assert_eq!(dur_x(ay), QUEUE_X + TEXT_W - 5);
    let (sx, _) = queue(&buf, "坂").unwrap();
    let (ox, _) = queue(&buf, "OREN").unwrap();
    assert_eq!(sx, ox, "artists line up: {shown}");
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Folder duration
// ---------------------------------------------------------------------------------------------

#[test]
fn folder_duration_shows_only_when_every_length_is_known() {
    common::isolate();
    let (mut app, dir) = lists_app("folderdur", &[0, 1]);
    let footnote = |buf: &Buffer| {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let (_, y) = tui::find_text(buf, &name).unwrap_or_else(|| panic!("{}", screen(buf)));
        span_text(buf, y, LIB_X, LIB_X + TEXT_W)
    };

    // One file's length is unknown: no total.
    let buf = tui::render(&mut app, 120, 40);
    let note = footnote(&buf);
    assert!(!note.contains("08:20"), "{note:?}");
    assert_eq!(note.matches('·').count(), 1, "path and position only: {note:?}");

    // Once it is known (tags arrive in the cache), the total appears.
    let (file, title, artist, secs) = FILES[2];
    app.metadata_cache.insert(meta(&dir, file, title, artist, secs));
    let buf = tui::render(&mut app, 120, 40);
    let note = footnote(&buf);
    assert!(note.trim_end().ends_with("·  14:12"), "242+258+352 s: {note:?}");

    // A folder with no audio files has no total either.
    let empty = dir.join("Sea Glass EP");
    app.navigate_browser(empty);
    wait_for_listing(&mut app);
    let buf = tui::render(&mut app, 120, 40);
    let (_, y) = tui::find_text(&buf, "Sea Glass EP").unwrap_or_else(|| panic!("{}", screen(&buf)));
    let note = span_text(&buf, y, LIB_X, LIB_X + TEXT_W);
    assert_eq!(note.matches('·').count(), 1, "{note:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn loading_row_counts_the_items_read_so_far() {
    common::isolate();
    let (mut app, dir) = lists_app("loading", &[0, 1, 2]);
    app.browser_loading = true;
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    let (x, y) = lib(&buf, "READING DIRECTORY  ·  6 ITEMS").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, y, LIB_X, LIB_X + 3), "···");
    assert_eq!(tui::cell_style(&buf, x, y).fg, Some(P.dim));
    app.browser_loading = false;
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn file_names_keep_their_case_in_filenames_mode() {
    common::isolate();
    let (mut app, dir) = lists_app("files", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 2));
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('v'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.settings_mgr.settings.metadata_view);
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    assert!(lib(&buf, "01 Tidewater.flac").is_some(), "{shown}");
    assert!(queue(&buf, "03 Halcyon Drift.mp3").is_some(), "{shown}");
    assert!(lib(&buf, "FILES").is_some(), "label note: {shown}");
    // Folder names are still UPPERCASE.
    assert!(lib(&buf, "LOW TIDE ATLAS/").is_some(), "{shown}");
    let _ = fs::remove_dir_all(&dir);
}
