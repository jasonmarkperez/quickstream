//! Tab 2 Queue & Library in the Telemetry style (mockups 2c, 3a, 3g): the LIBRARY and QUEUE
//! panels, their columns, the IN QUEUE / `▪` markers, the cursor, the spotlight, the folder
//! duration and the width rules. Tab 5 Playlists (2f): the PLAYLISTS and tracks panels, the
//! MISSING FILE rows, the file path footnote and the same width rules.

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
    assert_eq!(dx, LIB_X + 8, "after the marker and index columns");
    assert_eq!(span_text(&buf, dy, LIB_X, LIB_X + 8), "  DIR   ");
    assert_eq!(
        span_text(&buf, dy, dx + 15, LIB_X + TEXT_W).trim(),
        "",
        "no child count"
    );
    assert!(!tui::cell_style(&buf, dx, dy).add_modifier.contains(Modifier::REVERSED));
    assert_eq!(tui::cell_style(&buf, LIB_X + 2, dy).fg, Some(P.dim), "DIR is DIM");
    assert!(
        tui::cell_style(&buf, dx, dy).add_modifier.contains(Modifier::BOLD),
        "folder names are bold"
    );

    // Music Homes and parent rows.
    let (_, hy) = lib(&buf, "MUSIC HOMES").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, hy, LIB_X + 2, LIB_X + 3), "★");
    let (_, py) = lib(&buf, "PARENT").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, py, LIB_X + 2, LIB_X + 4), "..");

    // File rows: number, TITLE, ARTIST, format and duration against the right edge.
    // Tags mode sorts by artist and title: GLASS HARBOUR, TIDEWATER, then HALCYON DRIFT.
    let (tx, ty) = lib(&buf, "TIDEWATER").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(tx, LIB_X + 8, "{shown}");
    assert_eq!(span_text(&buf, ty, LIB_X + 2, LIB_X + 4), "02");
    assert_eq!(tui::cell_style(&buf, LIB_X + 2, ty).fg, Some(P.dim), "number is DIM");
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

    // Queue rows: marker, playing glyph, number, TITLE, ARTIST, duration against the right edge.
    let (qx, qy) = queue(&buf, "HALCYON DRIFT").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(qx, QUEUE_X + 8);
    assert_eq!(span_text(&buf, qy, QUEUE_X, QUEUE_X + 8), "    01  ");
    assert_eq!(
        tui::cell_style(&buf, QUEUE_X + 4, qy).fg,
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
    assert_eq!(span_text(&buf, gy, LIB_X + 2, LIB_X + 4), "▪ ", "{}", screen(&buf));
    assert_eq!(tui::cell_style(&buf, LIB_X + 2, gy).fg, Some(P.ink));
    let (_, ty) = lib(&buf, "TIDEWATER").unwrap();
    assert_eq!(
        span_text(&buf, ty, LIB_X + 2, LIB_X + 4),
        "02",
        "files keep their numbers"
    );

    let (_, qy) = queue(&buf, "GLASS HARBOUR").unwrap();
    assert_eq!(span_text(&buf, qy, QUEUE_X + 2, QUEUE_X + 6), "▶ 02");
    for x in [QUEUE_X + 2, QUEUE_X + 4, QUEUE_X + 8] {
        let style = tui::cell_style(&buf, x, qy);
        assert_eq!(style.fg, Some(P.ink), "playing row is INK");
        assert!(style.add_modifier.contains(Modifier::BOLD), "playing row is bold");
    }
    let (_, oy) = queue(&buf, "TIDEWATER").unwrap();
    assert!(!tui::cell_style(&buf, QUEUE_X + 8, oy)
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

/// Replacing the queue with a new one that has had as many changes must not reuse the cached
/// IN QUEUE set or total (PR #68 review: the revision used to be per queue).
#[test]
fn a_replaced_queue_with_the_same_change_count_refreshes_in_queue_and_the_total() {
    common::isolate();
    let (mut app, dir) = lists_app("replaced", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 0));
    app.queue.add_track(queue_track(&dir, 1));
    let buf = tui::render(&mut app, 120, 40);
    assert!(screen(&buf).contains("2 TRACKS  ·  08:20"), "{}", screen(&buf));

    app.queue = quickstream::queue::PlaybackQueue::new();
    app.queue.add_track(queue_track(&dir, 2));
    app.queue.add_track(queue_track(&dir, 2));
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    assert!(shown.contains("2 TRACKS  ·  11:44"), "{shown}");
    let (_, ty) = lib(&buf, "TIDEWATER").unwrap();
    assert!(
        span_text(&buf, ty, LIB_X, LIB_X + TEXT_W).contains("FLAC  04:02"),
        "no longer queued: {shown}"
    );
    let (_, hy) = lib(&buf, "HALCYON DRIFT").unwrap();
    assert!(
        span_text(&buf, hy, LIB_X, LIB_X + TEXT_W).contains("IN QUEUE  05:52"),
        "{shown}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A track over an hour long reads `1:15:00` in its queue row, in the queue total and in the
/// mini player strip alike.
#[test]
fn an_hour_long_track_reads_the_same_in_the_rows_the_total_and_the_strip() {
    common::isolate();
    let (mut app, dir) = lists_app("hour", &[0, 1, 2]);
    let mut long = queue_track(&dir, 0);
    long.duration_seconds = 4500.0;
    app.queue.add_track(long);
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    let (_, qy) = queue(&buf, "TIDEWATER").unwrap_or_else(|| panic!("{shown}"));
    assert!(span_text(&buf, qy, QUEUE_X, 120).contains("1:15:00"), "{shown}");
    assert!(shown.contains("1 TRACK  ·  1:15:00"), "{shown}");
    let strip = tui::find_row(&buf, "VOL").unwrap_or_else(|| panic!("{shown}"));
    assert!(tui::row(&buf, strip).contains("1:15:00"), "{shown}");
    assert!(!shown.contains("75:00") && !shown.contains("01:15:00"), "{shown}");
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

/// The unfocused pane's `▸` has a column of its own, so it never hides the playing track's `▶`
/// (queue) or `▪` (library), even though the queue cursor usually sits on the current track
/// (PR #68 review).
#[test]
fn the_unfocused_marker_never_hides_the_playing_glyph() {
    common::isolate();
    let (mut app, dir) = lists_app("markerplaying", &[0, 1, 2]);
    app.queue.add_track(queue_track(&dir, 0));
    app.queue.add_track(queue_track(&dir, 1));
    app.queue.set_current_index(1);
    app.queue_cursor = 1;
    let playing = app
        .browser_entries
        .iter()
        .position(|(_, p, _)| p.ends_with(FILES[1].0))
        .unwrap();
    app.browser_cursor = playing;

    // Library focused: the queue's cursor row is the playing one and shows both.
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    let (_, qy) = queue(&buf, "GLASS HARBOUR").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, qy, QUEUE_X, QUEUE_X + 6), "▸ ▶ 02", "{shown}");
    assert_eq!(tui::cell_style(&buf, QUEUE_X, qy).fg, Some(P.mid));
    let play = tui::cell_style(&buf, QUEUE_X + 2, qy);
    assert_eq!(play.fg, Some(P.ink));
    assert!(play.add_modifier.contains(Modifier::BOLD));

    // Queue focused: the library's cursor row is the playing file and shows both.
    app.handle_tab_key(KeyCode::Char('l'));
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    let (_, ly) = lib(&buf, "GLASS HARBOUR").unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, ly, LIB_X, LIB_X + 3), "▸ ▪", "{shown}");
    assert_eq!(tui::cell_style(&buf, LIB_X, ly).fg, Some(P.mid));
    assert_eq!(tui::cell_style(&buf, LIB_X + 2, ly).fg, Some(P.ink));

    // Every row keeps the same columns whichever pane has focus.
    let (tx, _) = lib(&buf, "TIDEWATER").unwrap();
    let (gx, _) = lib(&buf, "GLASS HARBOUR").unwrap();
    assert_eq!(tx, gx, "{shown}");
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

/// The `..` row (drawn `PARENT` / `FILE SYSTEM`) and the Music Homes row are not subfolders:
/// the spotlight never matches them by their stored labels (`.. [Parent Directory]`,
/// `.. [Back to File System]`) and the subfolder count leaves them out (PR #68 review).
#[test]
fn spotlight_matches_and_counts_only_real_subfolders() {
    common::isolate();
    let (mut app, dir) = lists_app("spotparent", &[0, 1, 2]);
    app.open_spotlight();
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    assert!(shown.contains("TYPE TO JUMP (3 SUBFOLDERS)"), "{shown}");

    let start = app.browser_cursor;
    for query in ["dir", "ent", "par", ".."] {
        app.browser_spotlight = Some(query.to_string());
        assert_eq!(app.spotlight_matches(), Vec::<usize>::new(), "{query}");
        app.jump_spotlight_to_first_match();
        assert_eq!(app.browser_cursor, start, "{query} must not jump to ..");
        let buf = tui::render(&mut app, 120, 40);
        assert!(
            screen(&buf).contains("NO MATCHING SUBFOLDERS"),
            "{query}: {}",
            screen(&buf)
        );
    }

    // Music Homes: `..` reads `FILE SYSTEM`; only the homes themselves are subfolders.
    app.bookmarks = vec![dir.join("Sea Glass EP").to_string_lossy().to_string()];
    app.in_bookmarks_view = true;
    app.refresh_browser();
    app.browser_spotlight = Some(String::new());
    let buf = tui::render(&mut app, 120, 40);
    assert!(screen(&buf).contains("TYPE TO JUMP (1 SUBFOLDERS)"), "{}", screen(&buf));
    for query in ["back", "file", "sys"] {
        app.browser_spotlight = Some(query.to_string());
        assert_eq!(app.spotlight_matches(), Vec::<usize>::new(), "{query}");
    }
    app.browser_spotlight = Some("sea".to_string());
    assert_eq!(app.spotlight_matches().len(), 1);
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

/// Tags arriving for files outside the browsed folder (the queue's metadata pool, autopilot)
/// leave its cached totals and columns alone; tags for one of its files recompute them once
/// (PR #68 review: any insert anywhere used to re-sum the whole folder every frame).
#[test]
fn folder_summary_is_recomputed_only_for_tags_of_its_own_files() {
    common::isolate();
    let (mut app, dir) = lists_app("foldersummary", &[0, 1]);
    tui::render(&mut app, 120, 40);
    let builds = app.folder_summary_builds();
    for i in 0..5 {
        let elsewhere = dir.join("Sea Glass EP");
        app.metadata_cache
            .insert(meta(&elsewhere, &format!("{i}.flac"), "Other", "Someone", 100.0));
        app.metadata_cache.insert(meta(
            Path::new("/elsewhere"),
            &format!("{i}.flac"),
            "Other",
            "Someone",
            100.0,
        ));
        tui::render(&mut app, 120, 40);
    }
    assert_eq!(app.folder_summary_builds(), builds, "no recompute for other folders");

    let (file, title, artist, secs) = FILES[2];
    app.metadata_cache.insert(meta(&dir, file, title, artist, secs));
    tui::render(&mut app, 120, 40);
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        app.folder_summary_builds(),
        builds + 1,
        "one recompute for its own file"
    );
    assert!(screen(&buf).contains("·  14:12"), "{}", screen(&buf));
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
    assert_eq!(span_text(&buf, y, LIB_X + 2, LIB_X + 5), "···");
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

// ---------------------------------------------------------------------------------------------
// Tab 5 Playlists (2f)
// ---------------------------------------------------------------------------------------------

/// At 120 columns the PLAYLISTS panel is 46 columns (text 44) and the tracks panel, 4 columns
/// after it, 66 (text 64), as in 2f.
const PL_X: u16 = 2;
const PL_TEXT_W: u16 = 44;
const TR_X: u16 = 52;
const TR_TEXT_W: u16 = 64;

fn pl_entry(dir: &Path, file: &str, artist: &str, title: &str, secs: i64) -> quickstream::playlist::PlaylistEntry {
    quickstream::playlist::PlaylistEntry {
        path: dir.join(file),
        duration_secs: secs,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

/// Replace the playlists folder with `Late Drives` (a tagged track, a missing one, one of
/// unknown length) and `Sunday Slow` (two tracks), and return an app on tab 5 whose existence
/// checks have finished: every file but `salt.flac` exists.
fn playlists_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_pl_lists_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let playlists_dir = quickstream::paths::get_playlists_dir();
    let _ = fs::remove_dir_all(&playlists_dir);
    fs::create_dir_all(&playlists_dir).unwrap();
    let write = |name: &str, entries| {
        let playlist = quickstream::playlist::Playlist {
            name: name.to_string(),
            entries,
        };
        fs::write(playlists_dir.join(format!("{name}.m3u8")), playlist.to_m3u8()).unwrap();
    };
    write(
        "Late Drives",
        vec![
            pl_entry(&dir, "northbound.flac", "Oren Vasquez", "Northbound, Slowly", 362),
            pl_entry(&dir, "salt.flac", "Ada Kowalczyk", "Salt Lines", 227),
            pl_entry(&dir, "sodium.flac", "", "Sodium Light", -1),
        ],
    );
    write(
        "Sunday Slow",
        vec![
            pl_entry(&dir, "a.flac", "Mira Lenz", "Tidewater", 242),
            pl_entry(&dir, "b.flac", "Mira Lenz", "Lanterns", 400),
        ],
    );
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    let checker: quickstream::ui::playlists::ExistsChecker =
        std::sync::Arc::new(|p: &Path| (!p.ends_with("salt.flac")).then(|| p.to_path_buf()));
    app.playlists.set_exists_checker(checker);
    app.active_tab = 4;
    let start = Instant::now();
    while !(app.playlists.store.is_loaded() && app.playlists.exists.len() == 3) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "playlists did not load and check"
        );
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(5));
    }
    (app, dir)
}

fn press(app: &mut AppUI, code: KeyCode) {
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ));
}

#[test]
fn playlists_2f_layout_at_120_cols() {
    common::isolate();
    let (mut app, dir) = playlists_app("2f");
    press(&mut app, KeyCode::Char('l'));
    assert!(app.playlists.tracks_focus);
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);

    // Label rows: PLAYLISTS (MID, unfocused) with the playlist count; the open playlist's name
    // (bold INK, focused) with its track count and the total of the known lengths (362 + 227).
    let (_, ly) = find_in(&buf, "PLAYLISTS", PL_X, PL_X + 1).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(tui::cell_style(&buf, PL_X, ly).fg, Some(P.mid), "{shown}");
    assert_eq!(
        find_in(&buf, "2", PL_X + PL_TEXT_W - 1, PL_X + PL_TEXT_W),
        Some((PL_X + PL_TEXT_W - 1, ly))
    );
    assert_eq!(tui::cell_style(&buf, PL_X + PL_TEXT_W - 1, ly).fg, Some(P.dim));
    assert_eq!(
        find_in(&buf, "LATE DRIVES", TR_X, TR_X + 1),
        Some((TR_X, ly)),
        "{shown}"
    );
    let label = tui::cell_style(&buf, TR_X, ly);
    assert_eq!(label.fg, Some(P.ink));
    assert!(label.add_modifier.contains(Modifier::BOLD));
    let note = "3 TRACKS  ·  09:49";
    let note_x = TR_X + TR_TEXT_W - note.chars().count() as u16;
    assert_eq!(find_in(&buf, note, note_x, note_x + 1), Some((note_x, ly)), "{shown}");
    assert_eq!(tui::cell_style(&buf, note_x, ly).fg, Some(P.dim));

    // Rules: the focused one INK, the other RULE; no box borders.
    assert_eq!(span_text(&buf, ly + 1, TR_X, TR_X + 3), "───");
    assert_eq!(tui::cell_style(&buf, TR_X, ly + 1).fg, Some(P.ink));
    assert_eq!(tui::cell_style(&buf, PL_X, ly + 1).fg, Some(P.rule));
    assert!(!shown.contains('│') && !shown.contains('┌'), "{shown}");

    // Playlist rows: name from column 2 of the panel, track count against the text edge (DIM);
    // the open playlist keeps `▸` while the tracks pane has focus.
    let (nx, ny) = find_in(&buf, "LATE DRIVES", PL_X, PL_X + 3).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(
        (nx, span_text(&buf, ny, PL_X, PL_X + 1)),
        (PL_X + 2, "▸".to_string()),
        "{shown}"
    );
    assert_eq!(span_text(&buf, ny, PL_X + PL_TEXT_W - 2, PL_X + PL_TEXT_W), " 3");
    let (sx, sy) = find_in(&buf, "SUNDAY SLOW", PL_X, PL_X + 3).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(sx, PL_X + 2);
    assert_eq!(span_text(&buf, sy, PL_X + PL_TEXT_W - 1, PL_X + PL_TEXT_W), "2");
    assert_eq!(tui::cell_style(&buf, PL_X + PL_TEXT_W - 1, sy).fg, Some(P.dim));
    assert_eq!(tui::cell_style(&buf, sx, sy).fg, Some(P.ink));
    assert_eq!(ny, sy - 1);

    // Track rows: number (DIM), TITLE (INK, 30 columns), ARTIST (MID), length at the text edge.
    let (tx, ty) = find_in(&buf, "NORTHBOUND, SLOWLY", TR_X, 120).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(ty, ny, "track rows start on the playlist rows' row: {shown}");
    assert_eq!(tx, TR_X + 5);
    assert_eq!(span_text(&buf, ty, TR_X, TR_X + 5), "01   ");
    assert_eq!(
        find_in(&buf, "OREN VASQUEZ", TR_X, 120),
        Some((TR_X + 5 + 30, ty)),
        "{shown}"
    );
    assert_eq!(
        find_in(&buf, "06:02", TR_X, 120),
        Some((TR_X + TR_TEXT_W - 5, ty)),
        "{shown}"
    );
    // The focused pane's cursor is the reverse-video bar, one column past the text each side.
    for x in [TR_X - 1, TR_X, TR_X + TR_TEXT_W] {
        let style = tui::cell_style(&buf, x, ty);
        assert_eq!(
            (style.fg, style.bg),
            (Some(P.ground), Some(P.ink)),
            "column {x}: {shown}"
        );
    }
    let (_, uy) = find_in(&buf, "SODIUM LIGHT", TR_X, 120).unwrap();
    assert_eq!(span_text(&buf, uy, TR_X, TR_X + 5), "03   ");
    assert_eq!(tui::cell_style(&buf, TR_X, uy).fg, Some(P.dim), "number is DIM");
    assert_eq!(tui::cell_style(&buf, TR_X + 5, uy).fg, Some(P.ink), "title is INK");
    assert_eq!(
        span_text(&buf, uy, TR_X + 5 + 30, TR_X + TR_TEXT_W).trim(),
        "",
        "no artist, unknown length left out: {shown}"
    );

    // The footnote names the playlist's file.
    assert!(shown.contains("Late Drives.m3u8"), "{shown}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_rows_read_missing_file_in_error_with_no_length() {
    common::isolate();
    let (mut app, dir) = playlists_app("missing");
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    let (tx, ty) = find_in(&buf, "SALT LINES", TR_X, 120).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(span_text(&buf, ty, TR_X, TR_X + 2), "02");
    assert_eq!(tui::cell_style(&buf, tx, ty).fg, Some(P.dim), "missing title is DIM");
    let (mx, my) = find_in(&buf, "MISSING FILE", TR_X, 120).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!((mx, my), (TR_X + 5 + 30, ty), "{shown}");
    assert!(
        (mx..mx + 12).all(|x| tui::cell_style(&buf, x, my).fg == Some(P.error)),
        "MISSING FILE is ERROR"
    );
    assert_eq!(
        find_in(&buf, "--:--", TR_X, 120),
        Some((TR_X + TR_TEXT_W - 5, ty)),
        "{shown}"
    );
    assert_eq!(tui::cell_style(&buf, TR_X + TR_TEXT_W - 1, ty).fg, Some(P.dim));
    // The artist and length of a missing file are not shown; nor is the old marker.
    assert!(!shown.contains("ADA KOWALCZYK"), "{shown}");
    assert!(!shown.contains("03:47"), "{shown}");
    assert!(!shown.contains("✗"), "{shown}");
    // Existing files are not marked.
    let (_, ny) = find_in(&buf, "NORTHBOUND, SLOWLY", TR_X, 120).unwrap();
    assert!(!tui::row(&buf, ny).contains("MISSING"), "{shown}");
    let _ = fs::remove_dir_all(&dir);
}

/// While a rename is unsettled the file still has its old name, and the footnote says so; once
/// the writer has reported the rename done it shows the new name (PR #68 review).
#[test]
fn playlist_footnote_names_the_old_file_until_a_rename_is_settled() {
    common::isolate();
    let (mut app, dir) = playlists_app("footnoterename");
    app.playlists.store.rename("Late Drives", "Night Drives").unwrap();
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    assert!(find_in(&buf, "NIGHT DRIVES", TR_X, TR_X + 1).is_some(), "{shown}");
    assert!(find_in(&buf, "Late Drives.m3u8", TR_X, 120).is_some(), "{shown}");

    let start = Instant::now();
    loop {
        app.poll_playlists();
        let buf = tui::render(&mut app, 120, 40);
        if find_in(&buf, "Night Drives.m3u8", TR_X, 120).is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{}", screen(&buf));
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playlist_footnote_is_the_file_path_in_its_own_case() {
    common::isolate();
    let (mut app, dir) = playlists_app("footnote");
    let buf = tui::render(&mut app, 120, 40);
    let shown = screen(&buf);
    let (fx, fy) = find_in(&buf, "Late Drives.m3u8", TR_X, 120).unwrap_or_else(|| panic!("{shown}"));
    assert!(!shown.contains("LATE DRIVES.M3U8"), "{shown}");
    assert_eq!(tui::cell_style(&buf, fx, fy).fg, Some(P.dim));
    let (_, ly) = find_in(&buf, "LATE DRIVES", TR_X, TR_X + 1).unwrap();
    assert_eq!(fy, ly + 2, "the row under the rule: {shown}");
    // The whole path, from the panel's text edge, when it fits.
    let path = quickstream::paths::get_playlists_dir().join("Late Drives.m3u8");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let expected = match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    if expected.chars().count() <= TR_TEXT_W as usize {
        assert_eq!(
            span_text(&buf, fy, TR_X, TR_X + TR_TEXT_W).trim_end(),
            expected,
            "{shown}"
        );
    } else {
        assert!(span_text(&buf, fy, TR_X, TR_X + 1) == "…", "{shown}");
    }

    // Another playlist: its own file.
    press(&mut app, KeyCode::Down);
    let buf = tui::render(&mut app, 120, 40);
    assert!(
        find_in(&buf, "Sunday Slow.m3u8", TR_X, 120).is_some(),
        "{}",
        screen(&buf)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playlists_page_heights_are_the_rows_each_list_shows() {
    common::isolate();
    let (mut app, dir) = playlists_app("pages");
    for i in 0..40 {
        let entry = pl_entry(&dir, &format!("t{i:02}.flac"), "", &format!("Song {i:02}"), 60);
        app.playlists.store.add("Late Drives", vec![entry]).unwrap();
    }
    for (w, h) in [(120, 40), (120, 30), (100, 30), (85, 30)] {
        let buf = tui::render(&mut app, w, h);
        let shown = (0..h).filter(|&y| tui::row(&buf, y).contains("SONG ")).count()
            + (0..h)
                .filter(|&y| {
                    let row = tui::row(&buf, y);
                    ["NORTHBOUND", "SALT LINES", "SODIUM LIGHT"]
                        .iter()
                        .any(|t| row.contains(t))
                })
                .count();
        assert_eq!(shown, app.playlists.tracks_page_height, "{w}x{h}:\n{}", screen(&buf));
        assert_eq!(
            app.playlists.list_page_height, app.playlists.tracks_page_height,
            "{w}x{h}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playlists_at_85_cols_drop_the_artist_column_and_the_path() {
    common::isolate();
    let (mut app, dir) = playlists_app("w85");
    let buf = tui::render(&mut app, 85, 30);
    let shown = screen(&buf);
    // Both panels, label rows side by side.
    let (_, ly) = find_in(&buf, "PLAYLISTS", PL_X, PL_X + 1).unwrap_or_else(|| panic!("{shown}"));
    let (lx, lsy) = find_in(&buf, "LATE DRIVES", PL_X + 10, 85).unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(lsy, ly, "{shown}");
    assert!(tui::row(&buf, ly).contains("3 TRACKS"), "{shown}");
    assert!(lx > PL_X + 20);
    // No artist column, no path footnote; titles keep their lengths and missing marks.
    assert!(!shown.contains("OREN VASQUEZ"), "{shown}");
    assert!(!shown.contains(".m3u8"), "{shown}");
    let (_, ty) = find_in(&buf, "NORTHBOUND", lx, 85).unwrap_or_else(|| panic!("{shown}"));
    assert!(tui::row(&buf, ty).contains("06:02"), "{shown}");
    let (_, my) = find_in(&buf, "SALT LINES", lx, 85).unwrap();
    let row = tui::row(&buf, my);
    assert!(row.contains("MISSING FILE") && row.contains("--:--"), "{shown}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playlists_under_80_cols_show_only_the_focused_pane() {
    common::isolate();
    let (mut app, dir) = playlists_app("w70");
    let starts_with = |buf: &Buffer, text: &str| {
        (0..buf.area.height).any(|y| span_text(buf, y, PL_X, buf.area.width).starts_with(text))
    };

    let buf = tui::render(&mut app, 70, 24);
    assert!(starts_with(&buf, "PLAYLISTS"), "{}", screen(&buf));
    assert!(tui::find_text(&buf, "SUNDAY SLOW").is_some(), "{}", screen(&buf));
    assert!(tui::find_text(&buf, "NORTHBOUND").is_none(), "{}", screen(&buf));

    press(&mut app, KeyCode::Char('l'));
    let buf = tui::render(&mut app, 70, 24);
    assert!(starts_with(&buf, "LATE DRIVES"), "{}", screen(&buf));
    assert!(!starts_with(&buf, "PLAYLISTS"), "{}", screen(&buf));
    assert!(tui::find_text(&buf, "NORTHBOUND").is_some(), "{}", screen(&buf));
    assert!(tui::find_text(&buf, "SUNDAY SLOW").is_none(), "{}", screen(&buf));
    // The hidden pane pages by the rows the shown one has.
    assert_eq!(app.playlists.list_page_height, app.playlists.tracks_page_height);

    press(&mut app, KeyCode::Char('h'));
    let buf = tui::render(&mut app, 70, 24);
    assert!(starts_with(&buf, "PLAYLISTS"), "{}", screen(&buf));
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Column layout is decided from the whole list, not the rows on screen
// ---------------------------------------------------------------------------------------------

/// Column of the first `…` (a cut title) in `from..to` of every list row: the rows whose text
/// there holds `row_text` (so labels and footnotes, cut too, are left out).
fn cut_columns(buf: &Buffer, from: u16, to: u16, row_text: &str) -> Vec<u16> {
    (0..buf.area.height)
        .filter(|&y| span_text(buf, y, from, to).contains(row_text))
        .filter_map(|y| (from..to).find(|&x| tui::cells(buf, y)[x as usize] == "…"))
        .collect()
}

/// Assert `a` and `b` are non-empty and every column in both is the same.
fn assert_same_cut(a: &[u16], b: &[u16], what: &str) {
    assert!(!a.is_empty() && !b.is_empty(), "{what}: no cut rows ({a:?}, {b:?})");
    let first = a[0];
    assert!(
        a.iter().chain(b).all(|&x| x == first),
        "{what}: the title column moved while scrolling ({a:?} vs {b:?})"
    );
}

const LONG: &str = "with a title far too long to fit in its column";

/// A queue whose first half has lengths and artists and whose second half doesn't (their tags
/// not read yet) keeps the same columns when scrolled (PR #68 review).
#[test]
fn queue_columns_do_not_change_while_scrolling() {
    common::isolate();
    let (mut app, dir) = lists_app("qscroll", &[0, 1, 2]);
    for i in 0..60 {
        let known = i < 30;
        app.queue.add_track(AudioMetadata {
            filepath: dir.join(format!("q{i:02}.flac")),
            filename: format!("q{i:02}.flac"),
            title: format!("Track {i:02} {LONG}"),
            artist: if known { "Mira Lenz".to_string() } else { String::new() },
            duration_seconds: if known { 200.0 } else { 0.0 },
            ..Default::default()
        });
    }
    app.handle_tab_key(KeyCode::Char('l'));
    app.queue_cursor = 0;
    let top = tui::render(&mut app, 120, 30);
    app.queue_cursor = 59;
    let bottom = tui::render(&mut app, 120, 30);
    assert!(queue(&top, "TRACK 00").is_some() && queue(&bottom, "TRACK 59").is_some());
    assert_same_cut(
        &cut_columns(&top, QUEUE_X, 120, "TRACK "),
        &cut_columns(&bottom, QUEUE_X, 120, "TRACK "),
        &format!("{}\n---\n{}", screen(&top), screen(&bottom)),
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The same for the library: tagged files (artist, length) sort after untagged ones, so the
/// first and last pages differ in what they have.
#[test]
fn library_columns_do_not_change_while_scrolling() {
    common::isolate();
    let dir = std::env::temp_dir().join(format!("quickstream_lists_libscroll_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let names: Vec<String> = (0..40).map(|i| format!("{i:02} a file name {LONG}.flac")).collect();
    for name in &names {
        fs::write(dir.join(name), b"").unwrap();
    }
    let mut settings = SettingsManager::with_path(dir.join("config.json"));
    settings.settings.metadata_view = false;
    let mut app = AppUI::new(Some(dir.clone()), settings);
    wait_for_listing(&mut app);
    for name in &names[20..] {
        app.metadata_cache
            .insert(meta(&dir, name, &format!("Song {LONG}"), "Mira Lenz", 200.0));
    }
    app.settings_mgr.settings.metadata_view = true;
    app.update_browser_entry_labels();
    app.active_tab = 1;

    app.browser_cursor = 0;
    let top = tui::render(&mut app, 120, 30);
    app.browser_cursor = app.browser_entries.len() - 1;
    let bottom = tui::render(&mut app, 120, 30);
    assert_same_cut(
        &cut_columns(&top, LIB_X, QUEUE_X - 2, "FLAC"),
        &cut_columns(&bottom, LIB_X, QUEUE_X - 2, "FLAC"),
        &format!("{}\n---\n{}", screen(&top), screen(&bottom)),
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Tab 5: the PLAYLISTS count column (one playlist of 100 tracks, the rest of 1) and the tracks
/// panel's artist and length columns (only the first half of the entries has them) keep their
/// widths while scrolling.
#[test]
fn playlist_columns_do_not_change_while_scrolling() {
    common::isolate();
    let dir = std::env::temp_dir().join(format!("quickstream_pl_lists_scroll_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let playlists_dir = quickstream::paths::get_playlists_dir();
    let _ = fs::remove_dir_all(&playlists_dir);
    fs::create_dir_all(&playlists_dir).unwrap();
    for p in 0..40 {
        let count = if p == 0 { 100 } else { 1 };
        let entries = (0..count)
            .map(|i| {
                let known = i < 50;
                pl_entry(
                    &dir,
                    &format!("p{p}_{i}.flac"),
                    if known { "Mira Lenz" } else { "" },
                    &format!("Track {i:02} {LONG}"),
                    if known { 200 } else { -1 },
                )
            })
            .collect();
        let name = format!("Playlist {p:02} with a name too long for the panel");
        let playlist = quickstream::playlist::Playlist { name, entries };
        fs::write(
            playlists_dir.join(format!("{}.m3u8", playlist.name)),
            playlist.to_m3u8(),
        )
        .unwrap();
    }
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    let checker: quickstream::ui::playlists::ExistsChecker = std::sync::Arc::new(|p: &Path| Some(p.to_path_buf()));
    app.playlists.set_exists_checker(checker);
    app.active_tab = 4;
    let start = Instant::now();
    while !app.playlists.store.is_loaded() {
        assert!(start.elapsed() < Duration::from_secs(5), "playlists did not load");
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(5));
    }

    // PLAYLISTS: the top page has the 100-track playlist, the bottom page doesn't.
    app.playlists.list_cursor = 0;
    let top = tui::render(&mut app, 120, 30);
    app.playlists.list_cursor = 39;
    let bottom = tui::render(&mut app, 120, 30);
    assert_same_cut(
        &cut_columns(&top, PL_X, PL_X + PL_TEXT_W, "PLAYLIST "),
        &cut_columns(&bottom, PL_X, PL_X + PL_TEXT_W, "PLAYLIST "),
        &format!("{}\n---\n{}", screen(&top), screen(&bottom)),
    );

    // Tracks of the 100-track playlist: artists and lengths only in the first half.
    app.playlists.list_cursor = 0;
    press(&mut app, KeyCode::Char('l'));
    app.playlists.track_cursor = 0;
    let top = tui::render(&mut app, 120, 30);
    app.playlists.track_cursor = 99;
    let bottom = tui::render(&mut app, 120, 30);
    assert!(find_in(&bottom, "TRACK 99", TR_X, 120).is_some(), "{}", screen(&bottom));
    assert_same_cut(
        &cut_columns(&top, TR_X, 120, "WITH A TITLE"),
        &cut_columns(&bottom, TR_X, 120, "WITH A TITLE"),
        &format!("{}\n---\n{}", screen(&top), screen(&bottom)),
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&playlists_dir);
}
