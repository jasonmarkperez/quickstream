//! The playlist modals in the Telemetry style: the name prompt (`c`, `R`, "+ New playlist…";
//! mockups 3b and 3c), the delete confirmation (`D`, 3d) and the add-to-playlist picker (`L`, 3e),
//! at 120x40 / 80x24 / 40x12.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use common::tui;
use quickstream::metadata::AudioMetadata;
use quickstream::paths;
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{NamePurpose, PendingAdd, PlaylistModal};
use quickstream::ui::theme;
use quickstream::ui::AppUI;

/// A fresh app with a loaded, empty playlists store, on tab 5. Tests in this binary share one
/// isolated config dir, so the playlists folder is removed first.
fn test_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_pl_modals_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let _ = fs::remove_dir_all(paths::get_playlists_dir());
    let mut settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    let mut app = AppUI::new(Some(dir.clone()), settings_mgr);
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.playlists.store.is_loaded(), "playlists did not load within 2s");
    app.active_tab = 4;
    (app, dir)
}

/// Send one key, upper-case letters with Shift as terminals do.
fn press(app: &mut AppUI, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap();
}

fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// Create playlist `name` with `tracks` entries (paths that need not exist).
fn seed(app: &mut AppUI, dir: &Path, name: &str, tracks: usize) {
    let stored = app.playlists.store.create(name).unwrap();
    let entries = (0..tracks)
        .map(|i| quickstream::playlist::PlaylistEntry::from_path(&dir.join(format!("{name}-{i}.flac"))))
        .collect();
    app.playlists.store.add(&stored, entries).unwrap();
}

/// The modal's outer rectangle: from the `┌` to the `┘` closing it.
fn modal_rect(buffer: &Buffer) -> Rect {
    let (x, y) = tui::find_text(buffer, "┌").unwrap_or_else(|| panic!("no modal\n{}", screen(buffer)));
    let right = (x + 1..buffer.area.width)
        .find(|&cx| buffer[(cx, y)].symbol() == "┐")
        .expect("no ┐");
    let bottom = (y + 1..buffer.area.height)
        .find(|&cy| buffer[(x, cy)].symbol() == "└")
        .expect("no └");
    assert_eq!(buffer[(right, bottom)].symbol(), "┘", "modal border not closed");
    Rect::new(x, y, right - x + 1, bottom - y + 1)
}

fn screen(buffer: &Buffer) -> String {
    tui::rows(buffer).join("\n")
}

/// The text of row `y` between the modal's side borders.
fn inside(buffer: &Buffer, rect: Rect, y: u16) -> String {
    (rect.x + 1..rect.right() - 1)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// Row of the modal (by its text inside the borders) that contains `text`.
fn modal_row(buffer: &Buffer, rect: Rect, text: &str) -> Option<u16> {
    (rect.y..rect.bottom()).find(|&y| inside(buffer, rect, y).contains(text))
}

/// The name prompt's field row: the one starting with its `NAME` label (the title row of the
/// rename prompt, `RENAME`, contains `NAME` too).
fn name_row(buffer: &Buffer, rect: Rect) -> Option<u16> {
    (rect.y..rect.bottom()).find(|&y| inside(buffer, rect, y).trim_start().starts_with("NAME "))
}

/// Column where `text` starts on row `y`, matching whole cells.
fn col_of(buffer: &Buffer, y: u16, text: &str) -> Option<u16> {
    let wanted: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    let cells = tui::cells(buffer, y);
    (0..=cells.len().saturating_sub(wanted.len()))
        .find(|&x| cells[x..x + wanted.len()] == wanted[..])
        .map(|x| x as u16)
}

/// The border cells of `rect` all have foreground `fg`.
fn assert_border(buffer: &Buffer, rect: Rect, fg: ratatui::style::Color) {
    for x in rect.left()..rect.right() {
        for y in [rect.top(), rect.bottom() - 1] {
            assert_eq!(buffer[(x, y)].fg, fg, "border ({x}, {y})\n{}", screen(buffer));
        }
    }
    for y in rect.top()..rect.bottom() {
        for x in [rect.left(), rect.right() - 1] {
            assert_eq!(buffer[(x, y)].fg, fg, "border ({x}, {y})\n{}", screen(buffer));
        }
    }
}

/// The cell at (`x`, `y`) has `expected`'s colours and modifiers.
#[track_caller]
fn assert_style(buffer: &Buffer, x: u16, y: u16, expected: ratatui::style::Style, what: &str) {
    let cell = &buffer[(x, y)];
    assert_eq!(
        (Some(cell.fg), Some(cell.bg), cell.modifier),
        (expected.fg, expected.bg, expected.add_modifier),
        "{what}: cell ({x}, {y})\n{}",
        screen(buffer)
    );
}

fn open_create_prompt(app: &mut AppUI, name: &str) {
    press(app, KeyCode::Char('c'));
    assert!(matches!(app.playlists.modal, Some(PlaylistModal::NamePrompt { .. })));
    type_text(app, name);
}

#[test]
fn name_prompt_matches_3b() {
    common::isolate();
    let (mut app, dir) = test_app("prompt_3b");
    open_create_prompt(&mut app, "Night Ferry");
    let p = theme::palette();
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    assert_eq!(rect.width, 64, "64 columns wide (3b)\n{}", screen(&buffer));
    assert_eq!(rect.x, (120 - 64) / 2, "centred");
    assert_border(&buffer, rect, p.dim);

    // Title chip three columns in, after a padding row.
    let title_y = modal_row(&buffer, rect, " NEW PLAYLIST ").expect("title chip");
    assert_eq!(title_y, rect.y + 2);
    let chip_x = col_of(&buffer, title_y, " NEW PLAYLIST ").unwrap();
    assert_eq!(chip_x, rect.x + 4);
    assert_style(&buffer, chip_x, title_y, theme::chip(false), "chip");

    // NAME (DIM, 7 columns) then the name in bold INK and the block cursor.
    let name_y = name_row(&buffer, rect).expect("NAME row");
    let label_x = col_of(&buffer, name_y, "NAME").unwrap();
    assert_eq!(label_x, rect.x + 4, "three columns in from the border");
    assert_eq!(buffer[(label_x, name_y)].fg, p.dim);
    let field_x = label_x + 7;
    assert_eq!(col_of(&buffer, name_y, "Night Ferry█"), Some(field_x));
    let name_style = tui::cell_style(&buffer, field_x, name_y);
    assert_eq!(name_style.fg, Some(p.ink));
    assert!(name_style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(buffer[(field_x + 11, name_y)].fg, p.ink, "cursor block");

    // The field's rule under it, INK, from the field to the padding.
    let rule_y = name_y + 1;
    assert_eq!(buffer[(field_x, rule_y)].symbol(), "─");
    assert_eq!(buffer[(field_x - 1, rule_y)].symbol(), " ");
    assert_eq!(buffer[(rect.right() - 5, rule_y)].symbol(), "─");
    assert_eq!(buffer[(rect.right() - 4, rule_y)].symbol(), " ", "3 columns of padding");
    assert_eq!(buffer[(field_x, rule_y)].fg, p.ink);

    // Hints after a blank row: ENTER SAVE, ESC CANCEL keycaps.
    let hints_y = modal_row(&buffer, rect, " ENTER  SAVE").expect("hints row");
    assert_eq!(hints_y, rule_y + 2);
    assert!(inside(&buffer, rect, rule_y + 1).trim().is_empty());
    let enter_x = col_of(&buffer, hints_y, " ENTER ").unwrap();
    assert_eq!(enter_x, rect.x + 4);
    assert_style(&buffer, enter_x, hints_y, theme::keycap(), "");
    assert!(inside(&buffer, rect, hints_y).contains(" ENTER  SAVE     ESC  CANCEL"));
    let save_x = col_of(&buffer, hints_y, "SAVE").unwrap();
    assert_eq!(buffer[(save_x, hints_y)].fg, p.mid);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rename_prompt_names_the_playlist_after_the_chip() {
    common::isolate();
    let (mut app, dir) = test_app("prompt_rename");
    seed(&mut app, &dir, "Late Drives", 2);
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Rename { .. },
            ..
        })
    ));
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    let title_y = modal_row(&buffer, rect, " RENAME ").expect("chip");
    let chip_x = col_of(&buffer, title_y, " RENAME ").unwrap();
    assert_style(&buffer, chip_x, title_y, theme::chip(false), "chip");
    // The playlist being renamed, as the panel shows it, in MID two columns after the chip.
    assert_eq!(col_of(&buffer, title_y, "LATE DRIVES"), Some(chip_x + 8 + 2));
    assert_eq!(buffer[(chip_x + 10, title_y)].fg, theme::palette().mid);
    // The field starts with the current name.
    let name_y = name_row(&buffer, rect).unwrap();
    assert!(
        inside(&buffer, rect, name_y).contains("NAME   Late Drives█"),
        "{}",
        screen(&buffer)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn name_prompt_error_replaces_the_hints_in_error_3c() {
    common::isolate();
    let (mut app, dir) = test_app("prompt_3c");
    seed(&mut app, &dir, "Focus", 0);
    seed(&mut app, &dir, "Late Drives", 0);
    let p = theme::palette();

    open_create_prompt(&mut app, "Focus");
    let before = modal_rect(&tui::render(&mut app, 120, 40));
    press(&mut app, KeyCode::Enter);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { error: Some(err), .. }) => assert!(err.contains("already exists")),
        _ => panic!("a duplicate name keeps the prompt open with an error"),
    }
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    assert_eq!(rect, before, "the error keeps the prompt's size and place");
    assert!(
        modal_row(&buffer, rect, "ENTER").is_none(),
        "the hints are gone\n{}",
        screen(&buffer)
    );
    let err_y = modal_row(&buffer, rect, "already exists").expect("error row");
    let name_y = name_row(&buffer, rect).unwrap();
    assert_eq!(err_y, name_y + 3, "on the hints row");
    let err_x = col_of(&buffer, err_y, "\"Focus\" already exists").expect("error text");
    assert_eq!(err_x, rect.x + 4);
    assert_eq!(buffer[(err_x, err_y)].fg, p.error);
    // The field's rule turns ERROR too.
    let field_x = col_of(&buffer, name_y, "NAME").unwrap() + 7;
    assert_eq!(buffer[(field_x, name_y + 1)].fg, p.error);

    // Typing clears the error and brings the hints back.
    type_text(&mut app, "2");
    let buffer = tui::render(&mut app, 120, 40);
    assert!(modal_row(&buffer, modal_rect(&buffer), " ENTER ").is_some());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn name_prompt_long_errors_show_whole() {
    common::isolate();
    let (mut app, dir) = test_app("prompt_long_error");
    let message = "A playlist file named \"Latin\" exists but couldn't be loaded";
    app.playlists.modal = Some(PlaylistModal::NamePrompt {
        purpose: NamePurpose::Create,
        input: "Latin".to_string(),
        error: Some(message.to_string()),
    });
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        let rect = modal_rect(&buffer);
        let text: Vec<String> = (rect.y..rect.bottom())
            .map(|y| inside(&buffer, rect, y).trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        assert!(
            text.join(" ").contains(message),
            "{w}x{h}: the whole message shows\n{}",
            screen(&buffer)
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn delete_confirmation_is_drawn_in_error_3d() {
    common::isolate();
    let (mut app, dir) = test_app("delete_3d");
    seed(&mut app, &dir, "Late Drives", 14);
    press(&mut app, KeyCode::Char('D'));
    assert!(matches!(app.playlists.modal, Some(PlaylistModal::ConfirmDelete { .. })));
    let p = theme::palette();
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    assert_eq!(rect.width, 64);
    assert_border(&buffer, rect, p.error);

    let title_y = modal_row(&buffer, rect, " DELETE PLAYLIST ").expect("chip");
    let chip_x = col_of(&buffer, title_y, " DELETE PLAYLIST ").unwrap();
    assert_eq!(chip_x, rect.x + 4);
    for x in chip_x..chip_x + 17 {
        assert_style(&buffer, x, title_y, theme::chip(true), &format!("chip cell {x}"));
    }
    assert_eq!(theme::chip(true).bg, Some(p.error));

    let q_y = modal_row(&buffer, rect, "Delete \"Late Drives\" (14 tracks)?").expect("question");
    assert!(
        !inside(&buffer, rect, q_y).contains("y/n"),
        "the keys are on their own row"
    );
    let q_x = col_of(&buffer, q_y, "Delete").unwrap();
    assert_eq!(q_x, rect.x + 4);
    assert!(tui::cell_style(&buffer, q_x, q_y).add_modifier.contains(Modifier::BOLD));
    assert!(
        modal_row(&buffer, rect, "The .m3u8 file is removed. Audio files are not touched.").is_some(),
        "{}",
        screen(&buffer)
    );

    // ` Y ` on ERROR, ` ANY KEY ` an ordinary keycap.
    let keys_y = modal_row(&buffer, rect, " Y  DELETE     ANY KEY  CANCEL").expect("keys");
    let y_x = col_of(&buffer, keys_y, " Y ").unwrap();
    for x in y_x..y_x + 3 {
        assert_eq!(buffer[(x, keys_y)].bg, p.error, "Y keycap cell {x}");
        assert_eq!(buffer[(x, keys_y)].fg, p.ground);
    }
    let any_x = col_of(&buffer, keys_y, " ANY KEY ").unwrap();
    assert_style(&buffer, any_x, keys_y, theme::keycap(), "");

    // Any key but y cancels.
    press(&mut app, KeyCode::Char('n'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.playlists.store.playlists().len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn delete_confirmation_keeps_the_track_count_for_long_names() {
    common::isolate();
    let (mut app, dir) = test_app("delete_long");
    app.playlists.modal = Some(PlaylistModal::ConfirmDelete {
        name: "A Very Long Playlist Name For Road Trips Across The Whole Country".to_string(),
        count: 3,
    });
    for (w, h) in [(120, 40), (80, 24), (40, 12)] {
        let buffer = tui::render(&mut app, w, h);
        let rect = modal_rect(&buffer);
        let row = modal_row(&buffer, rect, "(3 tracks)?").unwrap_or_else(|| panic!("{w}x{h}\n{}", screen(&buffer)));
        assert!(inside(&buffer, rect, row).contains("Delete \"A Very"), "{w}x{h}");
        assert!(inside(&buffer, rect, row).contains("…\" (3 tracks)?"), "{w}x{h}");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// An app on tab 2 with one queued track selected in the queue pane, so `L` adds it.
fn add_app(tag: &str) -> (AppUI, PathBuf) {
    let (mut app, dir) = test_app(tag);
    app.queue.add_track(AudioMetadata {
        filepath: dir.join("Afterimage.flac"),
        title: "Afterimage".to_string(),
        artist: "Mira Lenz".to_string(),
        ..Default::default()
    });
    app.active_tab = 1;
    app.queue_focus = true;
    (app, dir)
}

#[test]
fn add_picker_matches_3e() {
    common::isolate();
    let (mut app, dir) = add_app("add_3e");
    for (name, n) in [("Late Drives", 14), ("Sunday Slow", 22), ("Focus", 38)] {
        seed(&mut app, &dir, name, n);
    }
    app.settings_mgr.settings.last_playlist = Some("Late Drives".to_string());
    press(&mut app, KeyCode::Char('L'));
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::AddPicker { cursor: 1, .. })
    ));
    let p = theme::palette();
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    assert_eq!(rect.width, 52, "52 columns wide (3e)\n{}", screen(&buffer));
    assert_border(&buffer, rect, p.dim);
    let title_y = modal_row(&buffer, rect, " ADD TO PLAYLIST ").expect("chip");
    let chip_x = col_of(&buffer, title_y, " ADD TO PLAYLIST ").unwrap();
    assert_style(&buffer, chip_x, title_y, theme::chip(false), "chip");

    // ADDING (DIM, 8 columns), the title in bold, the artist in MID.
    let adding_y = modal_row(&buffer, rect, "ADDING").expect("header");
    let adding_x = col_of(&buffer, adding_y, "ADDING").unwrap();
    assert_eq!(adding_x, rect.x + 4);
    assert_eq!(buffer[(adding_x, adding_y)].fg, p.dim);
    assert_eq!(
        col_of(&buffer, adding_y, "AFTERIMAGE  ·  MIRA LENZ"),
        Some(adding_x + 8)
    );
    assert!(tui::cell_style(&buffer, adding_x + 8, adding_y)
        .add_modifier
        .contains(Modifier::BOLD));
    assert_eq!(buffer[(adding_x + 8 + 10 + 5, adding_y)].fg, p.mid);
    // Its rule `────── ▪`.
    let rule = inside(&buffer, rect, adding_y + 1);
    assert!(rule.trim_end().ends_with("─ ▪"), "{rule:?}");

    // One row per playlist (sorted), its track count in DIM at the right; the cursor bar on the
    // last playlist added to; then + NEW PLAYLIST… in MID.
    let rows: Vec<u16> = ["FOCUS", "LATE DRIVES", "SUNDAY SLOW", "+ NEW PLAYLIST…"]
        .iter()
        .map(|t| modal_row(&buffer, rect, t).unwrap_or_else(|| panic!("{t}\n{}", screen(&buffer))))
        .collect();
    assert!(rows.windows(2).all(|w| w[1] == w[0] + 1), "{rows:?}");
    assert_eq!(rows[0], adding_y + 3);
    let count_x = col_of(&buffer, rows[0], "38").unwrap();
    assert_eq!(count_x + 2, rect.right() - 4, "counts end at the padding");
    assert_eq!(buffer[(count_x, rows[0])].fg, p.dim);
    let late_x = col_of(&buffer, rows[1], "LATE DRIVES").unwrap();
    assert_eq!(late_x, rect.x + 4);
    for x in late_x - 1..rect.right() - 3 {
        assert_style(&buffer, x, rows[1], theme::cursor(), &format!("cursor cell {x}"));
    }
    let new_x = col_of(&buffer, rows[3], "+ NEW PLAYLIST…").unwrap();
    assert_eq!(buffer[(new_x, rows[3])].fg, p.mid);

    // Footer after a blank row.
    let keys_y = modal_row(&buffer, rect, " ENTER  ADD     J K  MOVE     ESC  CANCEL").expect("footer");
    assert_eq!(keys_y, rows[3] + 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn add_picker_names_a_folder_add() {
    common::isolate();
    let (mut app, dir) = test_app("add_folder");
    app.playlists.modal = Some(PlaylistModal::AddPicker {
        pending: PendingAdd::Folder {
            label: "Unplugged (folder)".to_string(),
            dir: dir.join("Unplugged"),
        },
        cursor: 0,
    });
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    assert!(
        modal_row(&buffer, rect, "ADDING  UNPLUGGED (FOLDER)").is_some(),
        "{}",
        screen(&buffer)
    );
    // With no playlists the only row is + NEW PLAYLIST…, under the cursor bar.
    let y = modal_row(&buffer, rect, "+ NEW PLAYLIST…").unwrap();
    let x = col_of(&buffer, y, "+ NEW").unwrap();
    assert_style(&buffer, x, y, theme::cursor(), "");
    let _ = fs::remove_dir_all(&dir);
}

/// Name of the add picker's selected row as drawn (upper case).
fn selected_row_text(app: &AppUI) -> String {
    let Some(PlaylistModal::AddPicker { cursor, .. }) = &app.playlists.modal else {
        panic!("add picker closed");
    };
    match app.playlists.store.playlists().get(*cursor) {
        Some(p) => p.name.to_uppercase(),
        None => "+ NEW PLAYLIST…".to_string(),
    }
}

#[test]
fn add_picker_scrolls_to_keep_the_cursor_visible() {
    common::isolate();
    let (mut app, dir) = add_app("add_scroll");
    for i in 0..40 {
        seed(&mut app, &dir, &format!("Mix {i:02}"), i);
    }
    // Preselected far down the list: shown on the first frame.
    app.settings_mgr.settings.last_playlist = Some("Mix 30".to_string());
    press(&mut app, KeyCode::Char('L'));
    for (w, h) in [(80, 24), (120, 40), (40, 12)] {
        let buffer = tui::render(&mut app, w, h);
        let rect = modal_rect(&buffer);
        assert!(rect.bottom() <= h && rect.height < h, "{w}x{h}: fits the screen");
        let y = modal_row(&buffer, rect, "MIX 30").unwrap_or_else(|| panic!("{w}x{h}\n{}", screen(&buffer)));
        let x = col_of(&buffer, y, "MIX 30").unwrap();
        assert_style(&buffer, x, y, theme::cursor(), &format!("{w}x{h}"));
        assert!(modal_row(&buffer, rect, "MIX 00").is_none(), "{w}x{h}: scrolled");
    }

    // Walk the whole list (wrapping once) at 80x24: the selected row always shows under the
    // cursor bar and the frame never moves.
    let first = modal_rect(&tui::render(&mut app, 80, 24));
    let mut seen_first = false;
    for _ in 0..45 {
        press(&mut app, KeyCode::Char('j'));
        let buffer = tui::render(&mut app, 80, 24);
        let rect = modal_rect(&buffer);
        assert_eq!(rect, first);
        let text = selected_row_text(&app);
        let y = modal_row(&buffer, rect, &text).unwrap_or_else(|| panic!("{text} hidden\n{}", screen(&buffer)));
        let x = col_of(&buffer, y, &text).unwrap();
        assert_style(&buffer, x, y, theme::cursor(), &text);
        seen_first |= text == "MIX 00";
        // The footer stays.
        assert!(modal_row(&buffer, rect, " ENTER ").is_some());
    }
    assert!(seen_first, "j wraps past + NEW PLAYLIST… to the first playlist");
    // k from the top wraps to + NEW PLAYLIST…, the last row.
    while selected_row_text(&app) != "MIX 00" {
        press(&mut app, KeyCode::Char('k'));
    }
    press(&mut app, KeyCode::Char('k'));
    let buffer = tui::render(&mut app, 80, 24);
    let rect = modal_rect(&buffer);
    let y = modal_row(&buffer, rect, "+ NEW PLAYLIST…").expect("last row shown");
    assert_style(&buffer, col_of(&buffer, y, "+").unwrap(), y, theme::cursor(), "");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playlist_modals_render_at_every_size_without_panicking() {
    common::isolate();
    let (mut app, dir) = add_app("sizes");
    for i in 0..12 {
        seed(&mut app, &dir, &format!("Mix {i:02}"), i);
    }
    let modals = [
        PlaylistModal::NamePrompt {
            purpose: NamePurpose::Create,
            input: "Night Ferry".to_string(),
            error: None,
        },
        PlaylistModal::NamePrompt {
            purpose: NamePurpose::Rename {
                old: "Mix 01".to_string(),
            },
            input: "Mix 02".to_string(),
            error: Some("\"Mix 02\" already exists".to_string()),
        },
        PlaylistModal::ConfirmDelete {
            name: "Mix 01".to_string(),
            count: 1,
        },
        PlaylistModal::AddPicker {
            pending: PendingAdd::Entries {
                label: "Afterimage".to_string(),
                entries: Vec::new(),
            },
            cursor: 11,
        },
    ];
    for modal in modals {
        app.playlists.modal = Some(modal.clone());
        for (w, h) in [(120, 40), (80, 24), (40, 12), (20, 6), (8, 4), (1, 1)] {
            let buffer = tui::render(&mut app, w, h);
            if (w, h) == (120, 40) || (w, h) == (80, 24) || (w, h) == (40, 12) {
                let rect = modal_rect(&buffer);
                assert!(rect.right() <= w && rect.bottom() <= h, "{modal:?} {w}x{h}");
                // Every modal keeps its main line at the sizes the app supports.
                let main = match &modal {
                    PlaylistModal::NamePrompt { .. } => "NAME",
                    PlaylistModal::ConfirmDelete { .. } => "Delete \"Mix 01\"",
                    PlaylistModal::AddPicker { .. } => "MIX 11",
                };
                assert!(
                    modal_row(&buffer, rect, main).is_some(),
                    "{modal:?} {w}x{h}\n{}",
                    screen(&buffer)
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}
