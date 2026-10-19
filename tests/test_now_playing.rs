//! Tab 1 Now Playing (mockups 2b at 120×40 and 3f at 80×24): keys, the NOW PLAYING panel (big
//! numerals, title block, progress bar), the SIGNAL panel or tech line, UP NEXT, and the
//! pending-skip and autopilot guarantees.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::buffer::Buffer;

use quickstream::analysis::{MixProfile, MoodType, TrackFeatures};
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::features::FeatureSource;
use quickstream::ui::{kit, theme, AppUI};

#[test]
fn test_now_playing_tab_key_navigation() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_keys_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Initial tab is 1 (Queue & Library)
    assert_eq!(app.active_tab, 1);

    // Press '1': switch to Now Playing (tab 0)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);

    // Press 'n': toggle to Queue & Library (tab 1)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 1);

    // Press 'n' again: toggle back to Now Playing (tab 0)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);

    // Numeric keys 2, 3, 4
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 2); // Devices
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 3); // Settings
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 1); // Queue

    // Tab cycling
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.active_tab, 2);

    // Esc is the global quit key, also on Now Playing (#57)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);
    app.browser_loading = false;
    let quit = app
        .handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(quit, "Esc on Now Playing should quit");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// The Now Playing key row (the row holding the `N` keycap) at `width` x 30.
fn now_playing_footer(app: &mut AppUI, width: u16) -> String {
    app.active_tab = 0;
    let buffer = common::tui::render(app, width, 30);
    let y = common::tui::find_row(&buffer, " N  QUEUE").unwrap_or_else(|| {
        panic!(
            "Now Playing key row at {width} columns:\n{}",
            common::tui::rows(&buffer).join("\n")
        )
    });
    common::tui::row(&buffer, y)
}

#[test]
fn test_now_playing_footer_names_n_not_esc_at_80_columns() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_footer_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Esc is the global quit key, so the key row must not offer it for switching tabs (#57),
    // and `N QUEUE` stays at every width.
    for width in [120, 100, 85, 80, 70, 40] {
        let footer = now_playing_footer(&mut app, width);
        assert!(footer.contains(" N  QUEUE"), "key row at {width} columns: {footer}");
        assert!(footer.contains(" ?  KEYS"), "key row at {width} columns: {footer}");
        assert!(
            !footer.to_uppercase().contains("ESC"),
            "key row at {width} columns: {footer}"
        );
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_now_playing_footer_shows_the_seek_step_setting() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_seek_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    app.settings_mgr.settings.seek_step_seconds = 30.0;
    let footer = now_playing_footer(&mut app, 120);
    assert!(footer.contains(" [ ]  SEEK 30S"), "footer: {footer}");
    let footer = now_playing_footer(&mut app, 100);
    assert!(footer.contains("SEEK 30S"), "footer: {footer}");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// A track in `dir` named `<title>.flac`, `secs` long (0 for unknown).
fn track(dir: &Path, title: &str, artist: &str, secs: f64) -> AudioMetadata {
    AudioMetadata {
        filepath: dir.join(format!("{title}.flac")),
        filename: format!("{title}.flac"),
        title: title.to_string(),
        artist: artist.to_string(),
        duration_seconds: secs,
        format_name: "flac".to_string(),
        sample_rate_hz: 96_000,
        ..Default::default()
    }
}

/// A fresh app on Now Playing with four tracks queued (the first current, 05:52, playing at
/// 02:41) and a features source that knows nothing, browsing a temp dir of its own.
fn np_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_np_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings_mgr);
    app.set_track_feature_source(Arc::new(|_: &Path| None));
    app.queue.add_track(AudioMetadata {
        album: "Low Tide Atlas".to_string(),
        bitrate_kbps: 2304,
        channels: 2,
        file_size: 50_541_363,
        ..track(&dir, "Halcyon Drift", "Mira Lenz", 352.0)
    });
    app.queue.add_track(track(&dir, "Glass Harbour", "Mira Lenz", 258.0));
    app.queue.add_track(AudioMetadata {
        sample_rate_hz: 44_100,
        ..track(&dir, "Northbound, Slowly", "Oren Vasquez", 362.0)
    });
    app.queue.add_track(AudioMetadata {
        format_name: "mp3".to_string(),
        sample_rate_hz: 44_100,
        bitrate_kbps: 320,
        ..track(&dir, "Salt Lines", "The Quiet Ward", 227.0)
    });
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(161.0, 352.0));
    app.active_tab = 0;
    (app, dir)
}

/// Features with BPM 112, key 8A (Am), energy 0.42, year 2023 and genre Ambient for every track.
fn fixed_features() -> FeatureSource {
    Arc::new(|path: &Path| {
        Some(TrackFeatures {
            filepath: path.to_path_buf(),
            bpm: Some(112.0),
            camelot_key: Some("8A".to_string()),
            initial_key: Some("Am".to_string()),
            energy: Some(0.42),
            year: Some(2023),
            genre: "Ambient".to_string(),
            ..TrackFeatures::default()
        })
    })
}

/// Poll the features worker until it has features for the current track (3 s at most).
fn wait_for_features(app: &mut AppUI) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.track_features().is_none() {
        assert!(Instant::now() < deadline, "the features worker found nothing");
        app.poll_track_features();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Where the big numerals of `text` start (their top row), if all three rows are drawn there.
fn numerals_at(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
    let rows = kit::big_digits(text);
    let (x, y) = common::tui::find_text(buffer, &rows[0])?;
    let n = rows[0].chars().count();
    for (dy, wanted) in rows.iter().enumerate().skip(1) {
        let cells = common::tui::cells(buffer, y + dy as u16);
        if cells.get(x as usize..x as usize + n)?.concat() != *wanted {
            return None;
        }
    }
    Some((x, y))
}

/// The SIGNAL panel's rows under its label, from the label's column on (trimmed).
fn signal_lines(buffer: &Buffer) -> Vec<String> {
    let (x, y) = common::tui::find_text(buffer, "SIGNAL").expect("SIGNAL panel");
    (y + 1..buffer.area.height)
        .map(|row| {
            common::tui::cells(buffer, row)[x as usize..]
                .concat()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The SIGNAL row labelled `label`, if shown.
fn signal_row(lines: &[String], label: &str) -> Option<String> {
    lines.iter().find(|l| l.starts_with(&format!("{label} "))).cloned()
}

/// The row holding the hero progress bar (`▕`; Now Playing has no strip).
fn bar_row(buffer: &Buffer) -> u16 {
    common::tui::find_row(buffer, "▕").unwrap_or_else(|| panic!("no bar:\n{}", common::tui::rows(buffer).join("\n")))
}

#[test]
fn test_now_playing_empty_state_says_no_track_loaded() {
    common::isolate();
    let (mut app, dir) = np_app("empty");
    app.queue.clear();
    for (w, h) in [(120, 40), (100, 30), (80, 24)] {
        let screen = common::tui::render_text(&mut app, w, h);
        assert!(screen.contains("NOW PLAYING"), "{w}x{h}:\n{screen}");
        assert!(screen.contains("NO TRACK LOADED"), "{w}x{h}:\n{screen}");
        assert!(screen.contains("Press Space to play"), "{w}x{h}:\n{screen}");
        assert!(!screen.contains("UP NEXT"), "{w}x{h}:\n{screen}");
        assert!(!screen.contains("SIGNAL"), "{w}x{h}:\n{screen}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_numerals_show_the_elapsed_time() {
    common::isolate();
    let (mut app, dir) = np_app("numerals");
    let buffer = common::tui::render(&mut app, 120, 40);
    let (x, y) = numerals_at(&buffer, "02:41")
        .unwrap_or_else(|| panic!("no 02:41 numerals:\n{}", common::tui::rows(&buffer).join("\n")));
    // Bold INK, and the total in normal digits after the bottom row.
    let style = common::tui::cell_style(&buffer, x, y);
    assert_eq!(style.fg, theme::ink().fg);
    assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
    let bottom = common::tui::row(&buffer, y + 2);
    assert!(bottom.contains("╹ / 05:52"), "{bottom:?}");

    // A seek in flight shows its target.
    app.pending_seek_target = Some(3605.0);
    let buffer = common::tui::render(&mut app, 120, 40);
    assert!(numerals_at(&buffer, "1:00:05").is_some(), "hours in the numerals");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_bar_fills_the_elapsed_share() {
    common::isolate();
    let (mut app, dir) = np_app("bar");
    let buffer = common::tui::render(&mut app, 120, 40);
    let y = bar_row(&buffer);
    let (mut elapsed, mut total) = (0usize, 0usize);
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, y)];
        if cell.symbol() == "╱" {
            total += 1;
            if cell.style().fg == theme::ink().fg {
                elapsed += 1;
            } else {
                assert_eq!(cell.style().fg, theme::rule().fg, "the rest is RULE");
            }
        }
    }
    // Full width: the bar spans both panels (2-column gutters, the percentage at the end).
    assert!(total >= 100, "bar {total} wide");
    let expected = (161.0 / 352.0 * total as f64).round() as usize;
    assert_eq!(elapsed, expected, "elapsed share of {total}");
    let row = common::tui::row(&buffer, y);
    assert!(row.trim_end().ends_with("46%"), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_signal_rows_without_data_are_omitted() {
    common::isolate();
    let (mut app, dir) = np_app("signal_none");
    // Only the format is known.
    let current = app.queue.track_mut(0).unwrap();
    current.sample_rate_hz = 0;
    current.bitrate_kbps = 0;
    current.channels = 0;
    current.file_size = 0;
    let buffer = common::tui::render(&mut app, 120, 40);
    let lines = signal_lines(&buffer);
    assert_eq!(
        signal_row(&lines, "FORMAT").as_deref().map(str::trim_end),
        Some("FORMAT    FLAC")
    );
    for label in ["RATE", "BITRATE", "CHANNELS", "GENRE", "BPM", "KEY", "ENERGY", "SIZE"] {
        assert!(
            signal_row(&lines, label).is_none(),
            "{label} without data:\n{}",
            lines.join("\n")
        );
    }
    let screen = common::tui::rows(&buffer).join("\n");
    assert!(!screen.contains("BIT /"), "no bit depth:\n{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_signal_rows_from_metadata_and_features() {
    common::isolate();
    let (mut app, dir) = np_app("signal_full");
    app.set_track_feature_source(fixed_features());
    app.poll_track_features();
    wait_for_features(&mut app);
    let buffer = common::tui::render(&mut app, 120, 40);
    let lines = signal_lines(&buffer);
    let value = |label: &str| {
        signal_row(&lines, label).unwrap_or_else(|| panic!("no {label} row:\n{}", lines.join("\n")))[label.len()..]
            .trim()
            .to_string()
    };
    assert_eq!(value("FORMAT"), "FLAC");
    assert_eq!(value("RATE"), "96 KHZ");
    assert_eq!(value("BITRATE"), "2304 KBPS");
    assert_eq!(value("CHANNELS"), "STEREO");
    assert_eq!(value("GENRE"), "AMBIENT");
    assert_eq!(value("BPM"), "112");
    assert_eq!(value("KEY"), "8A");
    assert_eq!(value("ENERGY"), "0.42");
    assert_eq!(value("SIZE"), "48.2 MB");
    // The year joins the artist / album line.
    let screen = common::tui::rows(&buffer).join("\n");
    assert!(screen.contains("MIRA LENZ  /  LOW TIDE ATLAS  /  2023"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_never_shows_the_previous_tracks_features() {
    common::isolate();
    let (mut app, dir) = np_app("signal_stale");
    let only_first: FeatureSource = Arc::new(|path: &Path| {
        path.ends_with("Halcyon Drift.flac").then(|| TrackFeatures {
            filepath: path.to_path_buf(),
            bpm: Some(112.0),
            ..TrackFeatures::default()
        })
    });
    app.set_track_feature_source(only_first);
    app.poll_track_features();
    wait_for_features(&mut app);
    let lines = signal_lines(&common::tui::render(&mut app, 120, 40));
    assert!(signal_row(&lines, "BPM").is_some(), "{}", lines.join("\n"));

    // The track changed and the worker hasn't been asked yet: Halcyon Drift's BPM is not shown
    // for Glass Harbour.
    app.queue.set_current_index(1);
    let lines = signal_lines(&common::tui::render(&mut app, 120, 40));
    assert!(signal_row(&lines, "BPM").is_none(), "{}", lines.join("\n"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_layout_at_120x40() {
    common::isolate();
    let (mut app, dir) = np_app("layout120");
    let buffer = common::tui::render(&mut app, 120, 40);
    let rows = common::tui::rows(&buffer);
    let screen = rows.join("\n");

    // NOW PLAYING on the left, SIGNAL on the right of the same label row.
    let (signal_x, label_y) = common::tui::find_text(&buffer, "SIGNAL").expect("SIGNAL");
    assert!(signal_x >= 80, "SIGNAL at column {signal_x}");
    assert!(rows[label_y as usize].starts_with("  NOW PLAYING"), "{screen}");
    let footnote = &rows[label_y as usize + 2];
    assert!(
        footnote.contains("TRACK 01/04  ·  QUEUE LOOP  ·  ▶ PLAYING"),
        "{footnote:?}"
    );
    assert!(footnote.contains("SOURCE LOCAL/"), "{footnote:?}");

    // Numerals, then the title and the artist / album line under them.
    let (_, numerals_y) = numerals_at(&buffer, "02:41").expect("numerals");
    let title_y = common::tui::find_row(&buffer, "HALCYON DRIFT").expect("title");
    assert!(title_y > numerals_y + 2, "{screen}");
    assert!(
        rows[title_y as usize + 1].contains("MIRA LENZ  /  LOW TIDE ATLAS"),
        "{screen}"
    );

    // The bar, then elapsed, remaining and the path in its own case.
    let bar_y = bar_row(&buffer);
    assert!(bar_y > title_y, "{screen}");
    let info = &rows[bar_y as usize + 1];
    assert!(info.contains("ELAPSED 02:41   REMAIN 03:11"), "{info:?}");
    assert!(info.contains("Halcyon Drift.flac"), "{info:?}");

    // UP NEXT below, with the format column.
    let up_y = common::tui::find_row(&buffer, "UP NEXT").expect("UP NEXT");
    assert!(up_y > bar_y + 1, "{screen}");
    assert!(rows[up_y as usize].contains("3 REMAINING  ·  14:07"), "{screen}");
    let row = |title: &str| rows.iter().find(|r| r.contains(title)).cloned().unwrap_or_default();
    let glass = row("GLASS HARBOUR");
    assert!(glass.contains("02") && glass.contains("MIRA LENZ"), "{glass:?}");
    assert!(
        glass.contains("FLAC 96") && glass.trim_end().ends_with("04:18"),
        "{glass:?}"
    );
    assert!(row("NORTHBOUND, SLOWLY").contains("FLAC 44.1"), "{screen}");
    assert!(row("SALT LINES").contains("MP3 320"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_layout_at_100x30() {
    common::isolate();
    let (mut app, dir) = np_app("layout100");
    let buffer = common::tui::render(&mut app, 100, 30);
    let rows = common::tui::rows(&buffer);
    let screen = rows.join("\n");

    assert!(
        !screen.contains("SIGNAL"),
        "no SIGNAL panel under 110 columns:\n{screen}"
    );
    assert!(screen.contains("TRACK 01/04  ·  QUEUE LOOP  ·  ▶ PLAYING"), "{screen}");
    // The title beside the numerals.
    let (_, numerals_y) = numerals_at(&buffer, "02:41").expect("numerals");
    assert!(rows[numerals_y as usize].contains("HALCYON DRIFT"), "{screen}");
    assert!(rows[numerals_y as usize + 1].contains("MIRA LENZ"), "{screen}");
    assert!(rows[numerals_y as usize + 2].contains("LOW TIDE ATLAS"), "{screen}");
    // The bar ends with the total; the tech line follows it.
    let bar_y = bar_row(&buffer) as usize;
    assert!(rows[bar_y].trim_end().ends_with("05:52"), "{:?}", rows[bar_y]);
    assert!(
        rows[bar_y + 1].contains("FLAC  96 KHZ  2304 KBPS  STEREO"),
        "{:?}",
        rows[bar_y + 1]
    );
    assert!(rows[bar_y + 2].contains("ELAPSED 02:41"), "{screen}");
    // UP NEXT keeps the artist but has no format column.
    let glass = rows.iter().find(|r| r.contains("GLASS HARBOUR")).expect("up next row");
    assert!(glass.contains("MIRA LENZ") && !glass.contains("FLAC"), "{glass:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_layout_at_80x24() {
    common::isolate();
    let (mut app, dir) = np_app("layout80");
    let buffer = common::tui::render(&mut app, 80, 24);
    let rows = common::tui::rows(&buffer);
    let screen = rows.join("\n");

    // Compact: position, mode and state at the right end of the label row.
    let label = rows.iter().find(|r| r.starts_with("  NOW PLAYING")).expect("label row");
    assert!(label.contains("01/04  ·  LOOP  ·  ▶ PLAYING"), "{label:?}");
    assert!(!screen.contains("TRACK 01/04"), "{screen}");
    let (_, numerals_y) = numerals_at(&buffer, "02:41").expect("numerals");
    assert!(rows[numerals_y as usize].contains("HALCYON DRIFT"), "{screen}");
    let bar_y = bar_row(&buffer) as usize;
    assert!(rows[bar_y].trim_end().ends_with("05:52"), "{screen}");
    assert!(rows[bar_y + 1].contains("FLAC  96 KHZ"), "{screen}");
    // UP NEXT with the remaining count and at least one track.
    let up = rows.iter().find(|r| r.contains("UP NEXT")).expect("UP NEXT");
    assert!(up.contains("3 REMAINING"), "{up:?}");
    assert!(screen.contains("GLASS HARBOUR"), "{screen}");
    // Every row keeps the 2-column gutters.
    for row in &rows {
        let cells: Vec<char> = row.chars().collect();
        assert!(
            cells.len() <= 2 || (cells[0] == ' ' && cells[cells.len() - 1] == ' '),
            "gutter: {row:?}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_up_next_total_only_when_every_length_is_known() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_total");
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    app.queue.track_mut(3).unwrap().duration_seconds = 0.0;
    let screen = common::tui::render_text(&mut app, 120, 40);
    let up = screen
        .lines()
        .find(|r| r.contains("UP NEXT"))
        .expect("UP NEXT")
        .to_string();
    assert!(up.trim_end().ends_with("3 REMAINING"), "{up:?}");

    // Playing the last track: nothing remains.
    app.queue.set_current_index(3);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("0 REMAINING"), "{screen}");
    assert!(screen.contains("PLAYBACK STOPS AFTER THIS TRACK"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_up_next_summary_is_cached_on_the_queue_revision() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_cache");
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    for _ in 0..5 {
        common::tui::render(&mut app, 120, 40);
    }
    assert_eq!(app.up_next_summary_builds(), 1);
    app.queue.set_current_index(1);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert_eq!(app.up_next_summary_builds(), 2);
    assert!(screen.contains("2 REMAINING  ·  09:49"), "{screen}");
    // A new queue mode or the continuous stream starting changes what plays next.
    app.settings_mgr.settings.queue_mode = "loop".to_string();
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert_eq!(app.up_next_summary_builds(), 3);
    assert!(screen.contains("3 REMAINING  ·  15:41"), "{screen}");
    common::tui::render(&mut app, 120, 40);
    assert_eq!(app.up_next_summary_builds(), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_up_next_shows_file_names_in_filenames_mode() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_files");
    app.settings_mgr.settings.metadata_view = false;
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("Glass Harbour.flac"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_drops_up_next_first_when_short() {
    common::isolate();
    let (mut app, dir) = np_app("short");
    for (w, h) in [(120, 20), (80, 20)] {
        let buffer = common::tui::render(&mut app, w, h);
        let screen = common::tui::rows(&buffer).join("\n");
        assert!(!screen.contains("UP NEXT"), "{w}x{h}:\n{screen}");
        assert!(numerals_at(&buffer, "02:41").is_some(), "{w}x{h}:\n{screen}");
        assert!(screen.contains("HALCYON DRIFT"), "{w}x{h}:\n{screen}");
        assert!(screen.contains("▶ PLAYING"), "{w}x{h}:\n{screen}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_footnote_follows_the_player_state() {
    common::isolate();
    let (mut app, dir) = np_app("state");
    app.player = Box::new(common::fake_player::FakePlayer {
        paused: true,
        position: 30.0,
        duration: 352.0,
        ..Default::default()
    });
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("‖ PAUSED"), "{screen}");
    app.player = Box::new(common::fake_player::FakePlayer::default());
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("■ STOPPED"), "{screen}");
    app.settings_mgr.settings.queue_mode = "repeat_one".to_string();
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("QUEUE REPEAT ONE"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_shows_a_pending_skip_as_queued_from_its_start() {
    common::isolate();
    let (mut app, dir) = np_app("skip");
    // Stop mode, so UP NEXT doesn't list Halcyon Drift after the loop.
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    let buffer = common::tui::render(&mut app, 120, 40);
    let screen = common::tui::rows(&buffer).join("\n");
    assert!(screen.contains("▶ PLAYING"), "{screen}");
    assert!(numerals_at(&buffer, "02:41").is_some(), "{screen}");

    // Right was pressed: the queue moved to Glass Harbour, the player still plays Halcyon Drift
    // until the debounce fires. The hero shows Glass Harbour from its start, marked queued, as
    // the strip does.
    app.queue.set_current_index(1);
    app.pending_track_skip = true;
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = common::tui::render(&mut app, w, h);
        let screen = common::tui::rows(&buffer).join("\n");
        assert!(screen.contains("» QUEUED"), "{w}x{h}:\n{screen}");
        assert!(!screen.contains("▶ PLAYING"), "{w}x{h}:\n{screen}");
        assert!(screen.contains("GLASS HARBOUR"), "{w}x{h}:\n{screen}");
        assert!(numerals_at(&buffer, "00:00").is_some(), "{w}x{h}:\n{screen}");
        assert!(numerals_at(&buffer, "02:41").is_none(), "{w}x{h}:\n{screen}");
        assert!(screen.contains("04:18"), "Glass Harbour's own length:\n{screen}");
        assert!(!screen.contains("05:52"), "Halcyon Drift's length is gone:\n{screen}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_pending_skip_never_shows_the_old_tracks_duration() {
    common::isolate();
    // Glass Harbour's length isn't known yet (e.g. added before its tags were read).
    let (mut app, dir) = np_app("skip_nodur");
    app.queue.track_mut(1).unwrap().duration_seconds = 0.0;
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(125.0, 300.0));

    // Right was pressed: the player still holds Halcyon Drift (reporting 5:00) until the
    // debounce fires.
    app.queue.set_current_index(1);
    app.pending_track_skip = true;
    let buffer = common::tui::render(&mut app, 120, 40);
    let screen = common::tui::rows(&buffer).join("\n");
    assert!(screen.contains("» QUEUED"), "{screen}");
    assert!(numerals_at(&buffer, "00:00").is_some(), "from 00:00:\n{screen}");
    assert!(!screen.contains("05:00"), "the old duration is not shown:\n{screen}");

    // The strip on tab 2 shows the same: from 00:00, no 05:00.
    app.active_tab = 1;
    let buffer = common::tui::render(&mut app, 120, 40);
    let y = common::tui::find_row(&buffer, "QUEUED").expect("strip");
    let strip = common::tui::row(&buffer, y);
    assert!(strip.contains("00:00") && !strip.contains("05:00"), "{strip:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_autopilot_badge_names_the_mix_profile() {
    common::isolate();
    let (mut app, dir) = np_app("profile");
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(!screen.contains("AUTOPILOT"), "no badge without autopilot: {screen}");

    app.autopilot_dir = Some(dir.clone());
    for (profile, name, short) in [
        (
            MixProfile::DiverseShuffle,
            "AUTOPILOT · DIVERSE SHUFFLE",
            "DIVERSE SHUFFLE",
        ),
        (
            MixProfile::BpmFlow {
                target_bpm: 120.0,
                tolerance: 0.15,
            },
            "AUTOPILOT · BPM FLOW",
            "BPM FLOW",
        ),
        (
            MixProfile::MoodVibe(MoodType::Chill),
            "AUTOPILOT · CHILL & AMBIENT",
            "CHILL & AMBIENT",
        ),
        (
            MixProfile::MoodVibe(MoodType::Workout),
            "AUTOPILOT · WORKOUT",
            "WORKOUT",
        ),
        (
            MixProfile::MoodVibe(MoodType::Groove),
            "AUTOPILOT · LATE NIGHT GROOVE",
            "LATE NIGHT GROOVE",
        ),
        (
            MixProfile::SeedRadio(dir.join("Halcyon Drift.flac")),
            "AUTOPILOT · SEED RADIO",
            "SEED RADIO",
        ),
    ] {
        app.active_mix_profile = profile;
        let screen = common::tui::render_text(&mut app, 120, 40);
        assert!(screen.contains(name), "{name:?} missing:\n{screen}");
        // At 80×24 the label row carries it beside the position and state.
        let screen = common::tui::render_text(&mut app, 80, 24);
        let label = screen
            .lines()
            .find(|r| r.starts_with("  NOW PLAYING"))
            .unwrap_or_default();
        // (`⚡` is two columns wide; the cell after it reads as a space.)
        assert!(label.contains(&format!("⚡  {short}")), "{short:?} missing:\n{screen}");
        assert!(label.contains("▶ PLAYING"), "{label:?}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_render_never_reads_features_itself() {
    common::isolate();
    let (mut app, dir) = np_app("no_db");
    // A slow source that records the threads it is called on.
    let callers = Arc::new(Mutex::new(Vec::new()));
    let seen = callers.clone();
    let slow: FeatureSource = Arc::new(move |path: &Path| {
        seen.lock().unwrap().push(std::thread::current().id());
        std::thread::sleep(Duration::from_millis(200));
        Some(TrackFeatures {
            filepath: path.to_path_buf(),
            bpm: Some(112.0),
            ..TrackFeatures::default()
        })
    });
    app.set_track_feature_source(slow);
    app.poll_track_features();

    let started = Instant::now();
    for _ in 0..10 {
        let lines = signal_lines(&common::tui::render(&mut app, 120, 40));
        if app.track_features().is_none() {
            assert!(signal_row(&lines, "BPM").is_none(), "no BPM before the worker has it");
        }
    }
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "10 frames took {:?}",
        started.elapsed()
    );

    wait_for_features(&mut app);
    let lines = signal_lines(&common::tui::render(&mut app, 120, 40));
    assert!(signal_row(&lines, "BPM").is_some(), "{}", lines.join("\n"));
    let me = std::thread::current().id();
    let callers = callers.lock().unwrap();
    assert!(!callers.is_empty());
    assert!(callers.iter().all(|id| *id != me), "features read on the UI thread");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_signal_rows_never_push_the_bar_off_a_short_screen() {
    common::isolate();
    let (mut app, dir) = np_app("short_signal");
    app.set_track_feature_source(fixed_features());
    app.poll_track_features();
    wait_for_features(&mut app);
    // Nine SIGNAL rows, 14 rows of screen (12 for the tab): the bar fits only if SIGNAL gives
    // up its last rows.
    let buffer = common::tui::render(&mut app, 120, 14);
    let screen = common::tui::rows(&buffer).join("\n");
    assert!(
        common::tui::find_row(&buffer, "▕").is_some(),
        "the bar stays:\n{screen}"
    );
    assert!(screen.contains("FORMAT"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_total_after_the_numerals_is_regular_dim() {
    common::isolate();
    let (mut app, dir) = np_app("total_style");
    let buffer = common::tui::render(&mut app, 120, 40);
    let styles = common::tui::text_styles(&buffer, " / 05:52")
        .unwrap_or_else(|| panic!("no total:\n{}", common::tui::rows(&buffer).join("\n")));
    for style in styles {
        assert_eq!(style.fg, theme::dim().fg, "DIM");
        assert!(
            !style.add_modifier.contains(ratatui::style::Modifier::BOLD),
            "regular weight, as in 2b: {style:?}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The UP NEXT rows from its label row down (trimmed), up to the end of the screen.
fn up_next_lines(app: &mut AppUI, w: u16, h: u16) -> Vec<String> {
    let buffer = common::tui::render(app, w, h);
    let rows = common::tui::rows(&buffer);
    let y = common::tui::find_row(&buffer, "UP NEXT")
        .unwrap_or_else(|| panic!("no UP NEXT:\n{}", rows.join("\n")));
    rows[y as usize..].iter().map(|r| r.trim().to_string()).collect()
}

/// Where the first UP NEXT line containing `text` is, if any.
fn line_with(lines: &[String], text: &str) -> Option<usize> {
    lines.iter().position(|l| l.contains(text))
}

/// What UP NEXT says on the last track in each queue mode.
#[test]
fn test_now_playing_end_of_queue_follows_the_queue_mode() {
    common::isolate();
    let (mut app, dir) = np_app("end_of_queue");
    app.queue.set_current_index(3);

    // Loop: the queue starts over, and the first track is next.
    app.settings_mgr.settings.queue_mode = "loop".to_string();
    let lines = up_next_lines(&mut app, 120, 40);
    let wrap = line_with(&lines, "END OF QUEUE  ·  LOOPS TO 01").unwrap_or_else(|| panic!("{lines:#?}"));
    let first = line_with(&lines, "HALCYON DRIFT").unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(first > wrap, "{lines:#?}");
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    let lines = up_next_lines(&mut app, 120, 40);
    assert!(line_with(&lines, "END OF QUEUE  ·  PLAYBACK STOPS AFTER THIS TRACK").is_some(), "{lines:#?}");
    // Repeat-one replays the track when it ends.
    app.settings_mgr.settings.queue_mode = "repeat_one".to_string();
    let lines = up_next_lines(&mut app, 120, 40);
    assert!(line_with(&lines, "REPEAT ONE  ·  THIS TRACK REPEATS").is_some(), "{lines:#?}");
    assert!(line_with(&lines, "STOPS").is_none(), "{lines:#?}");

    // So does loop with a single track.
    app.queue.clear();
    app.queue.add_track(track(&dir, "Halcyon Drift", "Mira Lenz", 352.0));
    app.settings_mgr.settings.queue_mode = "loop".to_string();
    let lines = up_next_lines(&mut app, 120, 40);
    assert!(line_with(&lines, "END OF QUEUE  ·  REPEATS THIS TRACK").is_some(), "{lines:#?}");
    let _ = fs::remove_dir_all(&dir);
}

/// Repeat-one with tracks after the current one: those don't play next, the current track does.
#[test]
fn test_now_playing_up_next_in_repeat_one_lists_no_later_tracks() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_repeat_one");
    app.settings_mgr.settings.queue_mode = "repeat_one".to_string();
    for (w, h) in [(120, 40), (80, 24)] {
        let lines = up_next_lines(&mut app, w, h);
        assert!(line_with(&lines, "REPEAT ONE  ·  THIS TRACK REPEATS").is_some(), "{w}x{h}: {lines:#?}");
        for later in ["GLASS HARBOUR", "NORTHBOUND", "SALT LINES"] {
            assert!(line_with(&lines, later).is_none(), "{w}x{h}: {later}: {lines:#?}");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Loop mode: the list runs past the end of the queue to the first track, marked where it wraps.
#[test]
fn test_now_playing_up_next_in_loop_wraps_around() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_loop");
    app.settings_mgr.settings.queue_mode = "loop".to_string();
    app.queue.set_current_index(2);
    let lines = up_next_lines(&mut app, 120, 40);
    assert!(lines[0].contains("3 REMAINING"), "{lines:#?}");
    let salt = line_with(&lines, "SALT LINES").unwrap_or_else(|| panic!("{lines:#?}"));
    let wrap = line_with(&lines, "END OF QUEUE  ·  LOOPS TO 01").unwrap_or_else(|| panic!("{lines:#?}"));
    let halcyon = line_with(&lines, "HALCYON DRIFT").unwrap_or_else(|| panic!("{lines:#?}"));
    let glass = line_with(&lines, "GLASS HARBOUR").unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(salt < wrap && wrap < halcyon && halcyon < glass, "{lines:#?}");
    assert!(lines[halcyon].starts_with("01"), "{lines:#?}");
    let _ = fs::remove_dir_all(&dir);
}

/// Stop mode: the tracks to the end of the queue, then where playback stops.
#[test]
fn test_now_playing_up_next_in_stop_mode_ends_where_playback_stops() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_stop");
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    app.queue.set_current_index(2);
    let lines = up_next_lines(&mut app, 120, 40);
    assert!(lines[0].contains("1 REMAINING"), "{lines:#?}");
    let salt = line_with(&lines, "SALT LINES").unwrap_or_else(|| panic!("{lines:#?}"));
    let stops = line_with(&lines, "END OF QUEUE  ·  PLAYBACK STOPS").unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(salt < stops, "{lines:#?}");
    assert!(line_with(&lines, "HALCYON DRIFT").is_none(), "{lines:#?}");
    let _ = fs::remove_dir_all(&dir);
}

/// With the UPnP continuous stream the queue plays on to its end whatever the mode, then the
/// stream reshuffles it and starts over: UP NEXT never says playback stops.
#[test]
fn test_now_playing_up_next_follows_the_continuous_stream() {
    common::isolate();
    let (mut app, dir) = np_app("up_next_continuous");
    let (tx, _rx) = std::sync::mpsc::channel();
    let state = Arc::new(std::sync::RwLock::new(quickstream::web::NowPlayingState::default()));
    let handle = quickstream::web::WebServer::start(0, state, tx).expect("web server");
    handle
        .stream_state()
        .continuous
        .is_active
        .store(true, std::sync::atomic::Ordering::SeqCst);
    app.web_server = Some(handle);

    for mode in ["stop", "repeat_one", "loop"] {
        app.settings_mgr.settings.queue_mode = mode.to_string();
        app.queue.set_current_index(2);
        let lines = up_next_lines(&mut app, 120, 40);
        assert!(line_with(&lines, "SALT LINES").is_some(), "{mode}: {lines:#?}");
        assert!(line_with(&lines, "HALCYON DRIFT").is_none(), "{mode}: {lines:#?}");
        assert!(line_with(&lines, "RESHUFFLES").is_some(), "{mode}: {lines:#?}");
        assert!(line_with(&lines, "STOPS").is_none(), "{mode}: {lines:#?}");
        assert!(line_with(&lines, "THIS TRACK REPEATS").is_none(), "{mode}: {lines:#?}");

        app.queue.set_current_index(3);
        let lines = up_next_lines(&mut app, 120, 40);
        assert!(
            line_with(&lines, "END OF QUEUE  ·  RESHUFFLES AND STARTS OVER").is_some(),
            "{mode}: {lines:#?}"
        );
    }
    if let Some(handle) = app.web_server.take() {
        handle.stop();
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A file whose tags couldn't be read (or haven't been yet) has only placeholder metadata: the
/// tab shows its format and size, takes its length from the player, and makes nothing up.
#[test]
fn test_now_playing_unread_tags_show_no_made_up_signal() {
    common::isolate();
    let (mut app, dir) = np_app("unread_tags");
    let current = dir.join("02 - cjk.ogg");
    let next = dir.join("03 - other.ogg");
    fs::write(&current, vec![0x55u8; 40_000]).unwrap();
    fs::write(&next, vec![0x55u8; 40_000]).unwrap();
    app.queue.clear();
    app.queue.add_track(quickstream::metadata::fallback_metadata(&current));
    app.queue.add_track(quickstream::metadata::fallback_metadata(&next));
    // mpv knows the real length.
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(4.0, 8.0));

    let buffer = common::tui::render(&mut app, 120, 40);
    let rows = common::tui::rows(&buffer);
    let screen = rows.join("\n");
    let lines = signal_lines(&buffer);
    assert!(
        signal_row(&lines, "FORMAT").is_some_and(|r| r.contains("OGG")),
        "{screen}"
    );
    assert!(signal_row(&lines, "SIZE").is_some(), "{screen}");
    for label in ["RATE", "BITRATE", "CHANNELS"] {
        assert!(signal_row(&lines, label).is_none(), "{label} made up:\n{screen}");
    }
    let (_, y) = numerals_at(&buffer, "00:04").expect("numerals");
    assert!(
        rows[y as usize + 2].contains("/ 00:08"),
        "the player's length:\n{screen}"
    );
    assert!(screen.contains("ELAPSED 00:04   REMAIN 00:04"), "{screen}");
    let bar = common::tui::row(&buffer, bar_row(&buffer));
    assert!(bar.trim_end().ends_with("50%"), "{bar:?}");
    let other = rows.iter().find(|r| r.contains("03 - OTHER")).expect("up next row");
    assert!(other.contains("OGG") && !other.contains("320"), "{other:?}");

    // Under 110 columns the tech line has only the format.
    let buffer = common::tui::render(&mut app, 100, 30);
    let rows = common::tui::rows(&buffer);
    let tech = rows[bar_row(&buffer) as usize + 1].trim().to_string();
    assert_eq!(tech, "OGG", "{}", rows.join("\n"));
    let _ = fs::remove_dir_all(&dir);
}

/// UP NEXT does not depend on whether the features have arrived: on a mid-height terminal the
/// SIGNAL rows that don't fit beside the hero are cut (least useful first) before UP NEXT goes.
#[test]
fn test_now_playing_up_next_stays_when_features_arrive_at_120x30() {
    common::isolate();
    let (mut app, dir) = np_app("signal_vs_up_next");
    let screen = common::tui::render_text(&mut app, 120, 30);
    assert!(screen.contains("UP NEXT"), "before the features:\n{screen}");

    app.set_track_feature_source(fixed_features());
    app.poll_track_features();
    wait_for_features(&mut app);
    let buffer = common::tui::render(&mut app, 120, 30);
    let screen = common::tui::rows(&buffer).join("\n");
    assert!(screen.contains("UP NEXT"), "after the features:\n{screen}");
    assert!(screen.contains("ELAPSED 02:41"), "the elapsed row stays:\n{screen}");
    // The most useful rows stay, in the 2b order; SIZE goes first.
    let lines = signal_lines(&buffer);
    for label in ["FORMAT", "RATE", "BPM", "KEY"] {
        assert!(signal_row(&lines, label).is_some(), "{label}:\n{screen}");
    }
    assert!(signal_row(&lines, "SIZE").is_none(), "{screen}");
    let shown: Vec<&str> = ["FORMAT", "RATE", "BITRATE", "CHANNELS", "GENRE", "BPM", "KEY", "ENERGY", "SIZE"]
        .into_iter()
        .filter(|l| signal_row(&lines, l).is_some())
        .collect();
    let positions: Vec<usize> = shown
        .iter()
        .map(|l| lines.iter().position(|r| r.starts_with(&format!("{l} "))).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "2b order:\n{screen}");
    let _ = fs::remove_dir_all(&dir);
}

/// Now Playing and the strip on the other tabs take the length from the same place: the
/// player's when it knows it (the tag length can be an estimate), else the track's own.
#[test]
fn test_now_playing_and_the_strip_show_the_same_length() {
    common::isolate();
    let (mut app, dir) = np_app("same_length");
    // The tags say 05:52; mpv says 06:00.
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(161.0, 360.0));
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains(" / 06:00"), "{screen}");
    assert!(screen.contains("REMAIN 03:19"), "{screen}");
    assert!(!screen.contains("05:52"), "{screen}");
    let screen = common::tui::render_text(&mut app, 80, 24);
    assert!(screen.contains("06:00") && !screen.contains("05:52"), "{screen}");

    app.active_tab = 1;
    let buffer = common::tui::render(&mut app, 120, 40);
    let strip_row = common::tui::rows(&buffer)
        .into_iter()
        .rev()
        .find(|r| r.contains("02:41") && r.contains("VOL"))
        .expect("strip");
    assert!(strip_row.contains("06:00"), "{strip_row:?}");

    // The player doesn't know the length yet: both take the track's own.
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(161.0, 0.0));
    let strip_row = common::tui::render_text(&mut app, 120, 40)
        .lines()
        .rev()
        .find(|r| r.contains("02:41") && r.contains("VOL"))
        .map(str::to_string)
        .expect("strip");
    assert!(strip_row.contains("05:52"), "{strip_row:?}");
    app.active_tab = 0;
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains(" / 05:52"), "{screen}");
    let _ = fs::remove_dir_all(&dir);
}
