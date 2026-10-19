//! The fuzzy search modal (`f`, mockup 2h) and the autopilot mix picker (`M`, mockup 2i): their
//! Telemetry layouts at 120x40 / 100x30 / 80x24, and that their keys still do what they did.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use common::tui;
use quickstream::analysis::{MixProfile, MoodType};
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::kit;
use quickstream::ui::search::SearchModalState;
use quickstream::ui::theme;
use quickstream::ui::AppUI;

fn test_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_search_mix_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    (AppUI::new(Some(dir.clone()), settings_mgr), dir)
}

#[allow(clippy::too_many_arguments)]
fn track(
    root: &Path,
    rel: &str,
    title: &str,
    artist: &str,
    album: &str,
    seconds: f64,
    format: &str,
    rate: u32,
    kbps: u32,
    channels: u8,
    size: u64,
) -> AudioMetadata {
    let path = root.join(rel);
    AudioMetadata {
        filename: path.file_name().unwrap().to_string_lossy().to_string(),
        filepath: path,
        title: title.to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        genre: String::new(),
        format_name: format.to_string(),
        duration_seconds: seconds,
        file_size: size,
        bitrate_kbps: kbps,
        sample_rate_hz: rate,
        channels,
    }
}

/// Six tracks: four match `halc`, one (`Bare Track`) has no tags beyond its title and no audio
/// properties.
fn library(root: &Path) -> Vec<AudioMetadata> {
    vec![
        track(
            root,
            "Mira Lenz/Low Tide Atlas/03 - Halcyon Drift.flac",
            "Halcyon Drift",
            "Mira Lenz",
            "Low Tide Atlas",
            352.0,
            "flac",
            96_000,
            2304,
            2,
            48_210_000,
        ),
        track(
            root,
            "Mira Lenz/Low Tide Atlas/09 - Halcyon (Reprise).flac",
            "Halcyon (Reprise)",
            "Mira Lenz",
            "Low Tide Atlas",
            108.0,
            "flac",
            96_000,
            2304,
            2,
            15_000_000,
        ),
        track(
            root,
            "The Quiet Ward/Halcyon Days.mp3",
            "Halcyon Days",
            "The Quiet Ward",
            "",
            261.0,
            "mp3",
            44_100,
            320,
            2,
            10_000_000,
        ),
        track(
            root,
            "Ada Kowalczyk/Half-Light Chorus.flac",
            "Half-Light Chorus",
            "Ada Kowalczyk",
            "",
            369.0,
            "flac",
            44_100,
            900,
            2,
            40_000_000,
        ),
        track(
            root,
            "Other/Unrelated Song.mp3",
            "Unrelated Song",
            "Someone Else",
            "",
            200.0,
            "mp3",
            44_100,
            256,
            2,
            6_000_000,
        ),
        track(
            root,
            "Loose/Bare Track.mp3",
            "Bare Track",
            "",
            "",
            0.0,
            "mp3",
            0,
            0,
            0,
            0,
        ),
    ]
}

fn open_search(app: &mut AppUI, root: &Path, query: &str) {
    let mut modal = SearchModalState::new(root.to_path_buf(), library(root));
    modal.update_query(query.to_string());
    app.search_modal = Some(modal);
}

fn screen(buffer: &Buffer) -> String {
    tui::rows(buffer).join("\n")
}

fn row_of(buffer: &Buffer, text: &str) -> String {
    let y = tui::find_row(buffer, text).unwrap_or_else(|| panic!("{text:?} not drawn\n{}", screen(buffer)));
    tui::row(buffer, y)
}

fn underlined(buffer: &Buffer, x: u16, y: u16) -> bool {
    buffer[(x, y)].modifier.contains(Modifier::UNDERLINED)
}

// ---------------------------------------------------------------------------------------------
// Search (2h)
// ---------------------------------------------------------------------------------------------

#[test]
fn search_title_row_has_the_search_chip_and_the_folder() {
    common::isolate();
    let (mut app, dir) = test_app("search_title");
    open_search(&mut app, &dir, "halc");
    let buffer = tui::render(&mut app, 120, 40);
    let p = theme::palette();

    let (x, y) = tui::find_text(&buffer, " SEARCH ").unwrap_or_else(|| panic!("no chip\n{}", screen(&buffer)));
    assert_eq!(buffer[(x, y)].bg, p.ink, "chip in reverse video");
    // The folder searched, in its own case, after the chip.
    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(tui::row(&buffer, y).contains(&dir_name), "{}", tui::row(&buffer, y));
    // No scan is running: no SCANNING note.
    assert!(!screen(&buffer).contains("SCANNING"), "{}", screen(&buffer));
    // The old title and boxes are gone.
    assert!(!screen(&buffer).contains("Fuzzy Search"), "{}", screen(&buffer));
    assert!(!screen(&buffer).contains("Track Details"), "{}", screen(&buffer));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_query_row_shows_the_query_a_block_cursor_and_the_counts() {
    common::isolate();
    let (mut app, dir) = test_app("search_query");
    open_search(&mut app, &dir, "halc");
    let buffer = tui::render(&mut app, 120, 40);
    let p = theme::palette();

    let row = row_of(&buffer, "QUERY");
    assert!(row.contains("QUERY  halc"), "query as typed: {row:?}");
    assert!(row.contains("4 / 6 MATCHED"), "{row:?}");
    let (qx, qy) = tui::find_text(&buffer, "QUERY").unwrap();
    assert_eq!(buffer[(qx, qy)].fg, p.dim, "QUERY in DIM");
    let (hx, _) = tui::find_text(&buffer, "halc").unwrap();
    assert!(buffer[(hx, qy)].modifier.contains(Modifier::BOLD), "query bold");
    // The block cursor right after the query.
    assert_eq!(buffer[(hx + 4, qy)].bg, p.ink, "block cursor");
    // The count in bold INK, the rest DIM.
    let (cx, _) = tui::find_text(&buffer, "4 / 6 MATCHED").unwrap();
    assert_eq!(buffer[(cx, qy)].fg, p.ink);
    assert!(buffer[(cx, qy)].modifier.contains(Modifier::BOLD));
    assert_eq!(buffer[(cx + 4, qy)].fg, p.dim, "total DIM");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_counts_group_thousands() {
    common::isolate();
    let (mut app, dir) = test_app("search_thousands");
    let tracks: Vec<AudioMetadata> = (0..2500)
        .map(|i| {
            track(
                &dir,
                &format!("a/{i}.mp3"),
                &format!("Song {i}"),
                "A",
                "",
                60.0,
                "mp3",
                44_100,
                320,
                2,
                1,
            )
        })
        .collect();
    let mut modal = SearchModalState::new(dir.clone(), tracks);
    modal.update_query(String::new());
    let shown = modal.filtered.len();
    app.search_modal = Some(modal);
    let buffer = tui::render(&mut app, 120, 40);
    let expected = format!("{},{:03} / 2,500 MATCHED", shown / 1000, shown % 1000);
    assert!(screen(&buffer).contains(&expected), "{expected:?}\n{}", screen(&buffer));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_shows_scanning_while_the_scan_runs() {
    common::isolate();
    let (mut app, dir) = test_app("search_scanning");
    let (tx, rx) = mpsc::channel::<Vec<AudioMetadata>>();
    app.search_modal = Some(SearchModalState::new_with_scanner(
        dir.clone(),
        Vec::new(),
        Some(rx),
        None,
    ));
    tx.send(library(&dir)).unwrap();
    app.search_modal.as_mut().unwrap().poll_background_scan();

    let buffer = tui::render(&mut app, 120, 40);
    let title = row_of(&buffer, " SEARCH ");
    assert!(title.contains("SCANNING "), "{title:?}");
    let (sx, sy) = tui::find_text(&buffer, "SCANNING").unwrap();
    assert_eq!(buffer[(sx, sy)].fg, theme::palette().mid, "SCANNING in MID");
    // Tracks streamed so far are already listed and counted.
    assert!(screen(&buffer).contains("6 / 6 MATCHED"), "{}", screen(&buffer));

    drop(tx);
    app.search_modal.as_mut().unwrap().poll_background_scan();
    let buffer = tui::render(&mut app, 120, 40);
    assert!(!screen(&buffer).contains("SCANNING"), "{}", screen(&buffer));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_ranked_matches_rows_have_title_artist_and_length() {
    common::isolate();
    let (mut app, dir) = test_app("search_rows");
    open_search(&mut app, &dir, "halc");
    let buffer = tui::render(&mut app, 120, 40);
    let p = theme::palette();

    let labels = row_of(&buffer, "RANKED MATCHES");
    assert!(labels.contains("TRACK"), "TRACK panel beside the matches: {labels:?}");
    for (title, artist, length) in [
        ("HALCYON DRIFT", "MIRA LENZ", "05:52"),
        ("HALCYON (REPRISE)", "MIRA LENZ", "01:48"),
        ("HALCYON DAYS", "THE QUIET WARD", "04:21"),
        ("HALF-LIGHT CHORUS", "ADA KOWALCZYK", "06:09"),
    ] {
        let row = row_of(&buffer, title);
        assert!(row.contains(artist) && row.contains(length), "{row:?}");
    }
    assert!(!screen(&buffer).contains("UNRELATED SONG"));
    // Plain rows: artist and length in MID.
    let selected = app.search_modal.as_ref().unwrap().selected_index;
    let ranked: Vec<String> = app
        .search_modal
        .as_ref()
        .unwrap()
        .filtered
        .iter()
        .map(|(_, t)| kit::upper(&t.artist))
        .collect();
    let plain = (0..ranked.len()).find(|&i| i != selected).unwrap();
    let plain_title = kit::upper(app.search_modal.as_ref().unwrap().filtered[plain].1.display_title());
    let (_, y) = tui::find_text(&buffer, &plain_title).unwrap();
    let row = tui::row(&buffer, y);
    let artist_col = row.find(&ranked[plain]).unwrap();
    let artist_x = row[..artist_col].chars().count() as u16;
    assert_eq!(buffer[(artist_x, y)].fg, p.mid, "artist MID: {row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_cursor_bar_follows_the_selection() {
    common::isolate();
    let (mut app, dir) = test_app("search_cursor");
    open_search(&mut app, &dir, "halc");
    let p = theme::palette();
    for step in 0..3 {
        let buffer = tui::render(&mut app, 80, 24);
        let modal = app.search_modal.as_ref().unwrap();
        let selected = kit::upper(modal.selected_track().unwrap().display_title());
        let (x, y) = tui::find_text(&buffer, &selected).unwrap();
        assert_eq!(buffer[(x, y)].bg, p.ink, "step {step}: cursor bar on {selected:?}");
        assert_eq!(buffer[(x, y)].fg, p.ground);
        // The bar reaches one column before the text.
        assert_eq!(buffer[(x - 1, y)].bg, p.ink, "bar reaches before the text");
        // Every other match row is plain.
        for (i, (_, t)) in modal.filtered.iter().enumerate() {
            if i == modal.selected_index {
                continue;
            }
            let (ox, oy) = tui::find_text(&buffer, &kit::upper(t.display_title())).unwrap();
            assert_eq!(buffer[(ox, oy)].bg, p.ground, "step {step}: {} not selected", t.title);
        }
        app.search_modal.as_mut().unwrap().move_selection_down();
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_underlines_the_matched_characters() {
    common::isolate();
    let (mut app, dir) = test_app("search_underline");
    open_search(&mut app, &dir, "halc");
    let buffer = tui::render(&mut app, 80, 24);

    // A plain row (not under the cursor) and the cursor row both underline H A L C.
    for title in ["HALCYON DRIFT", "HALCYON DAYS"] {
        let (x, y) = tui::find_text(&buffer, title).unwrap();
        for dx in 0..4 {
            assert!(underlined(&buffer, x + dx, y), "{title}: column {dx} underlined");
        }
        for dx in 4..title.len() as u16 {
            assert!(!underlined(&buffer, x + dx, y), "{title}: column {dx} not underlined");
        }
    }
    // A scattered match: the four characters the fuzzy matcher picked, H … C.
    let (x, y) = tui::find_text(&buffer, "HALF-LIGHT CHORUS").unwrap();
    let marked: Vec<u16> = (0..17).filter(|&dx| underlined(&buffer, x + dx, y)).collect();
    let letters: String = marked
        .iter()
        .map(|&dx| tui::cells(&buffer, y)[(x + dx) as usize].clone())
        .collect();
    assert_eq!(letters, "HALC", "{marked:?}");
    assert_eq!(marked[3], 11, "the C of CHORUS");

    // A query matched in the artist marks the artist column.
    let (mut app, dir2) = test_app("search_underline_artist");
    open_search(&mut app, &dir2, "kowal");
    let buffer = tui::render(&mut app, 80, 24);
    let (x, y) = tui::find_text(&buffer, "ADA KOWALCZYK").unwrap();
    assert!(!underlined(&buffer, x, y), "A of ADA");
    for dx in 4..9 {
        assert!(underlined(&buffer, x + dx, y), "KOWAL column {dx}");
    }
    let (tx, ty) = tui::find_text(&buffer, "HALF-LIGHT CHORUS").unwrap();
    assert!((0..17).all(|dx| !underlined(&buffer, tx + dx, ty)), "title not marked");

    // No query: nothing underlined.
    let (mut app, dir3) = test_app("search_underline_none");
    open_search(&mut app, &dir3, "");
    let buffer = tui::render(&mut app, 80, 24);
    let (x, y) = tui::find_text(&buffer, "HALCYON DRIFT").unwrap();
    assert!((0..13).all(|dx| !underlined(&buffer, x + dx, y)));
    for d in [dir, dir2, dir3] {
        let _ = fs::remove_dir_all(&d);
    }
}

#[test]
fn search_draws_only_the_rows_that_fit_and_scrolls_to_the_selection() {
    common::isolate();
    let (mut app, dir) = test_app("search_scroll");
    let tracks: Vec<AudioMetadata> = (0..200)
        .map(|i| {
            track(
                &dir,
                &format!("a/{i:03}.mp3"),
                &format!("Song {i:03}"),
                "A",
                "",
                60.0,
                "mp3",
                44_100,
                320,
                2,
                1,
            )
        })
        .collect();
    let mut modal = SearchModalState::new(dir.clone(), tracks);
    modal.update_query(String::new());
    app.search_modal = Some(modal);
    for (w, h) in [(120, 40), (80, 24)] {
        app.search_modal.as_mut().unwrap().home();
        let buffer = tui::render(&mut app, w, h);
        let text = screen(&buffer);
        assert!(text.contains("SONG 000"), "{text}");
        assert!(!text.contains("SONG 199"), "{text}");
        let shown = tui::rows(&buffer).iter().filter(|r| r.contains("SONG ")).count();
        assert!(shown > 3 && shown < 40, "{shown} rows at {w}x{h}");

        app.search_modal.as_mut().unwrap().end();
        let buffer = tui::render(&mut app, w, h);
        let text = screen(&buffer);
        assert!(text.contains("SONG 199"), "{text}");
        assert!(!text.contains("SONG 000"), "{text}");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The TRACK panel's row for `label`: the value after it, trimmed.
fn detail(buffer: &Buffer, label: &str) -> Option<String> {
    let (x, y) = tui::find_text(buffer, &format!("{label} "))?;
    let row: String = tui::cells(buffer, y)[x as usize + label.len()..].concat();
    Some(row.trim().trim_end_matches('│').trim().to_string())
}

#[test]
fn search_track_panel_shows_the_selected_tracks_fields() {
    common::isolate();
    let (mut app, dir) = test_app("search_detail");
    open_search(&mut app, &dir, "drift");
    let buffer = tui::render(&mut app, 120, 40);
    let p = theme::palette();

    assert_eq!(detail(&buffer, "TITLE").as_deref(), Some("HALCYON DRIFT"));
    assert_eq!(detail(&buffer, "ARTIST").as_deref(), Some("MIRA LENZ"));
    assert_eq!(detail(&buffer, "ALBUM").as_deref(), Some("LOW TIDE ATLAS"));
    assert_eq!(detail(&buffer, "DURATION").as_deref(), Some("05:52"));
    assert_eq!(detail(&buffer, "FORMAT").as_deref(), Some("FLAC"));
    assert_eq!(detail(&buffer, "RATE").as_deref(), Some("96 KHZ"));
    assert_eq!(detail(&buffer, "BITRATE").as_deref(), Some("2304 KBPS"));
    assert_eq!(detail(&buffer, "CHANNELS").as_deref(), Some("STEREO"));
    assert_eq!(detail(&buffer, "SIZE").as_deref(), Some("46.0 MB"));
    let (lx, ly) = tui::find_text(&buffer, "DURATION").unwrap();
    assert_eq!(buffer[(lx, ly)].fg, p.dim, "labels DIM");
    let (tx, ty) = tui::find_text(&buffer, "HALCYON DRIFT").unwrap();
    // The title in the TRACK panel (not the list row, which is first) is bold.
    let (dx, dy) = {
        let row = tui::cells(&buffer, ty);
        let after: String = row[tx as usize + 13..].concat();
        assert!(
            after.contains("HALCYON DRIFT"),
            "list row and TITLE share a row: {after:?}"
        );
        let off = row[tx as usize + 13..]
            .windows(13)
            .position(|w| w.concat() == "HALCYON DRIFT")
            .unwrap();
        (tx + 13 + off as u16, ty)
    };
    assert!(buffer[(dx, dy)].modifier.contains(Modifier::BOLD), "TITLE bold");

    // The path in its own case, under PATH, in MID.
    let (px, py) = tui::find_text(&buffer, "PATH").unwrap();
    assert_eq!(buffer[(px, py)].fg, p.dim);
    let text = screen(&buffer);
    assert!(text.contains("Halcyon Drift.flac"), "file name in its own case\n{text}");
    assert!(text.contains("Low Tide Atlas/"), "{text}");
    let (fx, fy) = tui::find_text(&buffer, "Halcyon Drift.flac").unwrap();
    assert!(fy > py, "path under PATH");
    assert_eq!(buffer[(fx, fy)].fg, p.mid);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_track_panel_leaves_out_fields_without_data() {
    common::isolate();
    let (mut app, dir) = test_app("search_detail_bare");
    open_search(&mut app, &dir, "bare");
    let buffer = tui::render(&mut app, 120, 40);
    let text = screen(&buffer);
    assert_eq!(detail(&buffer, "TITLE").as_deref(), Some("BARE TRACK"));
    assert_eq!(detail(&buffer, "FORMAT").as_deref(), Some("MP3"));
    for label in ["ARTIST", "ALBUM", "DURATION", "RATE", "BITRATE", "CHANNELS", "SIZE"] {
        assert!(detail(&buffer, label).is_none(), "{label} without data\n{text}");
    }
    assert!(!text.contains("Unknown"), "{text}");
    assert!(!text.contains("00:00"), "no made-up length\n{text}");
    assert!(text.contains("Bare Track.mp3"), "{text}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_track_panel_only_from_90_columns() {
    common::isolate();
    let (mut app, dir) = test_app("search_widths");
    open_search(&mut app, &dir, "drift");
    for (w, h, panel) in [
        (120, 40, true),
        (100, 30, true),
        (90, 30, true),
        (89, 30, false),
        (80, 24, false),
    ] {
        let buffer = tui::render(&mut app, w, h);
        let text = screen(&buffer);
        assert!(text.contains("RANKED MATCHES"), "{w}x{h}\n{text}");
        assert!(text.contains("HALCYON DRIFT"), "{w}x{h}\n{text}");
        assert_eq!(text.contains("DURATION"), panel, "TRACK panel at {w}x{h}\n{text}");
        assert_eq!(
            tui::find_text(&buffer, "TRACK").is_some_and(|(_, y)| tui::row(&buffer, y).contains("RANKED MATCHES")),
            panel,
            "{w}x{h}\n{text}"
        );
        assert!(text.contains("1 / 6 MATCHED"), "{w}x{h}\n{text}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_footer_lists_the_modal_keys() {
    common::isolate();
    let (mut app, dir) = test_app("search_footer");
    open_search(&mut app, &dir, "halc");
    let p = theme::palette();
    for (w, h) in [(120, 40), (100, 30)] {
        let buffer = tui::render(&mut app, w, h);
        let row = row_of(&buffer, " ADD & CLOSE");
        for text in [
            " ENTER ",
            " TAB ",
            " ADD TO QUEUE",
            " ⇧TAB ",
            " ADD ALL (4)",
            " ESC ",
            " CANCEL",
        ] {
            assert!(row.contains(text), "{text:?} at {w}x{h}: {row:?}");
        }
        let (kx, ky) = tui::find_text(&buffer, " ⇧TAB ").unwrap();
        assert_eq!(buffer[(kx, ky)].bg, p.rule, "keycap");
    }
    // 80x24 keeps at least Enter and Esc.
    let buffer = tui::render(&mut app, 80, 24);
    let row = row_of(&buffer, " ADD & CLOSE");
    assert!(row.contains(" ESC "), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_with_no_matches_says_so() {
    common::isolate();
    let (mut app, dir) = test_app("search_empty");
    open_search(&mut app, &dir, "zzzzqq");
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        let text = screen(&buffer);
        assert!(text.contains("No tracks match your query."), "{text}");
        assert!(text.contains("0 / 6 MATCHED"), "{text}");
        assert!(!text.contains("DURATION"), "{text}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_never_panics_on_small_terminals() {
    common::isolate();
    let (mut app, dir) = test_app("search_small");
    open_search(&mut app, &dir, "halc");
    for (w, h) in [(70, 24), (40, 12), (20, 6), (90, 10), (120, 12), (8, 4)] {
        tui::render(&mut app, w, h);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_colours_stay_on_the_palette() {
    common::isolate();
    let (mut app, dir) = test_app("search_palette");
    open_search(&mut app, &dir, "halc");
    let p = theme::palette();
    let allowed = [p.ground, p.ink, p.mid, p.dim, p.rule];
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        for y in 0..h {
            for x in 0..w {
                let cell = &buffer[(x, y)];
                assert!(
                    allowed.contains(&cell.fg) && allowed.contains(&cell.bg),
                    "({x}, {y}) {:?}/{:?}\n{}",
                    cell.fg,
                    cell.bg,
                    screen(&buffer)
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Mix picker (2i)
// ---------------------------------------------------------------------------------------------

fn press(app: &mut AppUI, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap();
}

/// What the picker should show for each profile, built from `MixProfile` itself.
fn expected_rows() -> Vec<(String, String)> {
    MixProfile::picker_profiles()
        .iter()
        .map(|p| (kit::upper(&p.title()), p.description()))
        .collect()
}

#[test]
fn mix_profile_text_comes_from_the_profile_data() {
    common::isolate();
    let profiles = MixProfile::picker_profiles();
    assert_eq!(profiles.len(), 5);
    assert_eq!(profiles[0], MixProfile::DiverseShuffle);
    assert!(matches!(profiles[1], MixProfile::BpmFlow { .. }));
    assert_eq!(profiles[2], MixProfile::MoodVibe(MoodType::Chill));
    assert_eq!(profiles[3], MixProfile::MoodVibe(MoodType::Workout));
    assert_eq!(profiles[4], MixProfile::MoodVibe(MoodType::Groove));

    // BPM Flow's title is its name, with the target BPM.
    assert_eq!(profiles[1].title(), profiles[1].name());
    // Moods: the mood's name, and the BPM range from bpm_range() in the description.
    for mood in [MoodType::Chill, MoodType::Workout, MoodType::Groove] {
        let profile = MixProfile::MoodVibe(mood);
        assert_eq!(profile.title(), mood.name());
        let (lo, hi) = mood.bpm_range();
        let bpm = format!("({lo:.0}-{hi:.0} BPM)");
        assert!(profile.description().ends_with(&bpm), "{:?}", profile.description());
    }
    for profile in &profiles {
        assert!(!profile.description().is_empty());
        assert_eq!(
            profile.picker_index(),
            profiles.iter().position(|p| p == profile).unwrap()
        );
    }
    // Profiles the picker doesn't list put the cursor on a row it does.
    assert_eq!(MixProfile::SeedRadio(PathBuf::from("/a.mp3")).picker_index(), 0);
    assert_eq!(MixProfile::MoodVibe(MoodType::Focus).picker_index(), 4);
}

#[test]
fn mix_picker_rows_match_the_profiles() {
    common::isolate();
    let (mut app, dir) = test_app("mix_rows");
    press(&mut app, KeyCode::Char('M'));
    assert!(app.show_mix_picker_modal);
    let p = theme::palette();
    for (w, h) in [(120, 40), (100, 30), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        let text = screen(&buffer);
        assert!(text.contains(" AUTOPILOT MIX "), "{text}");
        assert!(
            text.contains("Sequences the queue with DJ spacing and transitions."),
            "{text}"
        );
        let mut last_y = 0;
        for (i, (title, description)) in expected_rows().iter().enumerate() {
            let (x, y) = tui::find_text(&buffer, title).unwrap_or_else(|| panic!("{title} at {w}x{h}\n{text}"));
            assert!(y > last_y, "rows in order at {w}x{h}");
            last_y = y;
            let row = tui::row(&buffer, y);
            let index = (i + 1).to_string();
            let ix = row.find(&format!("{index}  ")).expect("index") as u16;
            let ix = row[..ix as usize].chars().count() as u16;
            assert!(ix < x, "index before the title: {row:?}");
            if i != app.mix_picker_cursor {
                assert_eq!(buffer[(ix, y)].fg, p.dim, "index {index} DIM at {w}x{h}");
            }
            let next = tui::row(&buffer, y + 1);
            assert!(
                next.contains(description.as_str()),
                "{description:?} under {title} at {w}x{h}: {next:?}"
            );
        }
        assert!(
            !text.contains("[r]") && !text.contains("Seed Radio"),
            "no r hint\n{text}"
        );
        assert!(!text.contains("60-95"), "{text}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mix_picker_marks_the_current_profile() {
    common::isolate();
    let (mut app, dir) = test_app("mix_current");
    for (index, profile) in MixProfile::picker_profiles().into_iter().enumerate() {
        app.active_mix_profile = profile.clone();
        app.show_mix_picker_modal = false;
        press(&mut app, KeyCode::Char('M'));
        assert_eq!(app.mix_picker_cursor, index, "cursor on the current profile");
        let buffer = tui::render(&mut app, 120, 40);
        for (i, (title, _)) in expected_rows().iter().enumerate() {
            let row = row_of(&buffer, title);
            if i == index {
                assert!(row.contains('●') && !row.contains('○'), "{row:?}");
                assert!(row.contains("ACTIVE"), "{row:?}");
            } else {
                assert!(row.contains('○') && !row.contains('●'), "{row:?}");
                assert!(!row.contains("ACTIVE"), "{row:?}");
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mix_picker_cursor_bar_on_the_cursor_row() {
    common::isolate();
    let (mut app, dir) = test_app("mix_cursor");
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.mix_picker_cursor, 1);
    let p = theme::palette();
    let buffer = tui::render(&mut app, 120, 40);
    let rows = expected_rows();
    let (x, y) = tui::find_text(&buffer, &rows[1].0).unwrap();
    assert_eq!(buffer[(x, y)].bg, p.ink);
    assert_eq!(buffer[(x, y)].fg, p.ground);
    // Its description in MID, the others in DIM.
    let (dx, dy) = tui::find_text(&buffer, &rows[1].1).unwrap();
    assert_eq!(buffer[(dx, dy)].fg, p.mid);
    let (ox, oy) = tui::find_text(&buffer, &rows[2].1).unwrap();
    assert_eq!(buffer[(ox, oy)].fg, p.dim);
    let (tx, ty) = tui::find_text(&buffer, &rows[0].0).unwrap();
    assert_eq!(buffer[(tx, ty)].bg, p.ground);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mix_picker_footer_has_its_keys() {
    common::isolate();
    let (mut app, dir) = test_app("mix_footer");
    press(&mut app, KeyCode::Char('M'));
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        let row = row_of(&buffer, " START MIX");
        for text in [" J K ", " SELECT", " ENTER ", " ESC ", " CANCEL"] {
            assert!(row.contains(text), "{text:?} at {w}x{h}: {row:?}");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mix_picker_keys_are_unchanged() {
    common::isolate();
    let (mut app, dir) = test_app("mix_keys");
    // j / k wrap.
    press(&mut app, KeyCode::Char('M'));
    assert_eq!(app.mix_picker_cursor, 0);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.mix_picker_cursor, 4);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.mix_picker_cursor, 0);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.mix_picker_cursor, 4);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.mix_picker_cursor, 0);
    // Esc, q and M close.
    for close in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('M')] {
        assert!(app.show_mix_picker_modal);
        press(&mut app, close);
        assert!(!app.show_mix_picker_modal, "{close:?} closes");
        press(&mut app, KeyCode::Char('M'));
    }
    // A digit picks that profile; Enter picks the cursor's.
    press(&mut app, KeyCode::Char('3'));
    assert!(!app.show_mix_picker_modal);
    assert_eq!(app.active_mix_profile, MixProfile::MoodVibe(MoodType::Chill));
    press(&mut app, KeyCode::Char('M'));
    assert_eq!(app.mix_picker_cursor, 2);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    assert!(!app.show_mix_picker_modal);
    assert_eq!(app.active_mix_profile, MixProfile::MoodVibe(MoodType::Workout));
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(app.active_mix_profile, MixProfile::picker_profiles()[1]);
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mix_picker_never_panics_on_small_terminals() {
    common::isolate();
    let (mut app, dir) = test_app("mix_small");
    press(&mut app, KeyCode::Char('M'));
    for (w, h) in [(70, 24), (40, 12), (20, 6), (84, 25), (8, 4)] {
        tui::render(&mut app, w, h);
    }
    let _ = fs::remove_dir_all(&dir);
}
