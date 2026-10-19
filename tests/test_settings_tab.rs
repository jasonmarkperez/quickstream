//! Tab 4 Settings in the Telemetry style (mockup 2e at 120×40, width rules 3h): the grouped
//! PLAYBACK / LIBRARY / WEB list in display order, the cursor bar with `◂ VALUE ▸`, the config
//! file footnote, the option panel (●/○ and a description) from 110 columns, the web address in
//! its own case, the inline startup home editor, the key row, and small terminals.

mod common;

use std::fs;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use common::fake_player::FakePlayer;
use common::tui;
use quickstream::settings::{SettingGroup, SettingsManager, SETTING_DEFINITIONS, SETTING_DISPLAY_ORDER};
use quickstream::ui::theme::Palette;
use quickstream::ui::{kit, AppUI};

const P: Palette = Palette::TRUECOLOR;

/// At 120 columns (2e): the list's labels start at the gutter and its values 32 columns later;
/// the cursor bar covers columns 1 to 72; the option panel starts 4 columns after the 72-column
/// SETTINGS panel, its choices after a 4-column ●/○ column.
const LABEL_X: u16 = 2;
const VALUE_X: u16 = 34;
const BAR_END_X: u16 = 72;
const OPTION_X: u16 = 78;
const CHOICE_X: u16 = 82;

const ENGINE: usize = 1;
const QUEUE_MODE: usize = 4;
const WEB_SERVER: usize = 6;
const HOME_DIR: usize = 7;
const WEB_LAN: usize = 11;

/// An app on tab 4 whose settings live in `<tmp>/<tag>/Config.JSON` (mixed case, to check the
/// footnote keeps it), with a fake player so no engine starts.
fn settings_app(tag: &str) -> AppUI {
    let root = common::isolate();
    let dir = root.join(format!("settings_tab_{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings = SettingsManager::with_path(dir.join("Config.JSON"));
    let mut app = AppUI::new(Some(dir), settings);
    app.player = Box::new(FakePlayer {
        volume: 65,
        backend: Some("mpv"),
        ..FakePlayer::default()
    });
    app.active_tab = 3;
    app
}

/// Turn the web server on from its row, as the user does, on a port the OS picks, and return the
/// port it bound. Network access stays off, so it binds loopback only (no firewall prompt).
fn start_loopback_web(app: &mut AppUI) -> u16 {
    app.settings_mgr.settings.web_server_lan = false;
    app.settings_mgr.settings.web_server_port = 0;
    app.settings_cursor = WEB_SERVER;
    press(app, KeyCode::Right);
    app.web_port().expect("web server started")
}

fn press(app: &mut AppUI, code: KeyCode) -> bool {
    app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
}

fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        assert!(!press(app, KeyCode::Char(c)), "{c:?} must be text, not quit");
    }
}

fn dump(buf: &Buffer) -> String {
    tui::rows(buf).join("\n")
}

fn row_of(buf: &Buffer, text: &str) -> u16 {
    tui::find_row(buf, text).unwrap_or_else(|| panic!("no row with {text:?}:\n{}", dump(buf)))
}

/// The text of row `y` from column `from` to column `to` (exclusive), trimmed.
fn span(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    (from..to.min(buf.area.width))
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect::<String>()
        .trim()
        .to_string()
}

/// The SETTINGS panel's label row (the tab row also reads `4 SETTINGS`, further right).
fn label_row(buf: &Buffer) -> u16 {
    (0..buf.area.height)
        .find(|&y| span(buf, y, LABEL_X, LABEL_X + 9) == "SETTINGS")
        .unwrap_or_else(|| panic!("no SETTINGS label:\n{}", dump(buf)))
}

/// The list row of setting `index` (its label at the gutter), if it is on screen.
fn setting_row(buf: &Buffer, index: usize) -> Option<u16> {
    let label = kit::upper(SETTING_DEFINITIONS[index].label);
    (0..buf.area.height).find(|&y| span(buf, y, LABEL_X, VALUE_X) == label)
}

fn must_row(buf: &Buffer, index: usize) -> u16 {
    setting_row(buf, index).unwrap_or_else(|| panic!("no row for {}:\n{}", SETTING_DEFINITIONS[index].label, dump(buf)))
}

fn is_cursor(buf: &Buffer, x: u16, y: u16) -> bool {
    let style = tui::cell_style(buf, x, y);
    style.bg == Some(P.ink) && style.fg == Some(P.ground)
}

fn bold(buf: &Buffer, x: u16, y: u16) -> bool {
    tui::cell_style(buf, x, y).add_modifier.contains(Modifier::BOLD)
}

/// Where `text` starts at or right of column `from`.
fn text_from(buf: &Buffer, text: &str, from: u16) -> Option<(u16, u16)> {
    let wanted: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    (0..buf.area.height).find_map(|y| {
        let row = tui::cells(buf, y);
        let last = row.len().checked_sub(wanted.len())?;
        (usize::from(from)..=last)
            .find(|&x| row[x..x + wanted.len()] == wanted[..])
            .map(|x| (x as u16, y))
    })
}

fn hints(app: &AppUI) -> Vec<(String, String)> {
    app.tab_key_hints().into_iter().map(|h| (h.keys, h.label)).collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(k, l)| (k.to_string(), l.to_string())).collect()
}

// ---------- the grouped list ----------

#[test]
fn settings_list_is_grouped_in_display_order() {
    common::isolate();
    let mut app = settings_app("groups");
    let buf = tui::render(&mut app, 120, 40);

    let top = label_row(&buf);
    assert!(bold(&buf, LABEL_X, top), "focused panel label is bold");
    // Label, rule, footnote, blank, then the list.
    let first = top + 4;
    let mut want: Vec<String> = Vec::new();
    let mut group: Option<SettingGroup> = None;
    for &index in &SETTING_DISPLAY_ORDER {
        let def = &SETTING_DEFINITIONS[index];
        if group != Some(def.group) {
            if group.is_some() {
                want.push(String::new());
            }
            want.push(def.group.label().to_string());
            group = Some(def.group);
        }
        want.push(kit::upper(def.label));
    }
    let got: Vec<String> = (first..first + want.len() as u16)
        .map(|y| span(&buf, y, LABEL_X, VALUE_X))
        .collect();
    assert_eq!(got, want, "\n{}", dump(&buf));

    for heading in ["PLAYBACK", "LIBRARY", "WEB"] {
        let y = (first..first + want.len() as u16)
            .find(|&y| span(&buf, y, LABEL_X, VALUE_X) == heading)
            .unwrap();
        assert_eq!(tui::cell_style(&buf, LABEL_X, y).fg, Some(P.dim), "{heading} is DIM");
    }
    // Labels MID, values INK (off the cursor row).
    let seek = must_row(&buf, 2);
    assert_eq!(tui::cell_style(&buf, LABEL_X, seek).fg, Some(P.mid));
    assert_eq!(span(&buf, seek, VALUE_X, BAR_END_X), "10 S");
    assert_eq!(tui::cell_style(&buf, VALUE_X, seek).fg, Some(P.ink));
    assert_eq!(span(&buf, must_row(&buf, 0), VALUE_X, BAR_END_X), "TAGS");
    assert_eq!(
        span(&buf, must_row(&buf, QUEUE_MODE), VALUE_X, BAR_END_X),
        "CONTINUOUS (LOOP)"
    );
    assert_eq!(
        span(&buf, must_row(&buf, HOME_DIR), VALUE_X, BAR_END_X),
        "LAUNCH DIRECTORY"
    );
    assert_eq!(span(&buf, must_row(&buf, WEB_SERVER), VALUE_X, BAR_END_X), "OFF");
    assert_eq!(span(&buf, must_row(&buf, WEB_LAN), VALUE_X, BAR_END_X), "THIS COMPUTER");
}

#[test]
fn selected_row_is_a_cursor_bar_with_arrows_round_its_value() {
    common::isolate();
    let mut app = settings_app("cursor");
    let buf = tui::render(&mut app, 120, 40);

    // The cursor starts on the top row: Local Playback Engine.
    let engine = must_row(&buf, ENGINE);
    assert_eq!(span(&buf, engine, VALUE_X, BAR_END_X + 1), "◂ MPV ▸");
    assert!((1..=BAR_END_X).all(|x| is_cursor(&buf, x, engine)), "\n{}", dump(&buf));
    assert!(!is_cursor(&buf, BAR_END_X + 1, engine));
    let seek = must_row(&buf, 2);
    assert!(!is_cursor(&buf, LABEL_X, seek));
    assert_eq!(
        tui::find_text(&buf, "◂").map(|(_, y)| y),
        Some(engine),
        "one row has the arrows"
    );

    press(&mut app, KeyCode::Char('j'));
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(span(&buf, must_row(&buf, 2), VALUE_X, BAR_END_X + 1), "◂ 10 S ▸");
    assert_eq!(span(&buf, must_row(&buf, ENGINE), VALUE_X, BAR_END_X + 1), "MPV");
    assert!(is_cursor(&buf, LABEL_X, must_row(&buf, 2)));

    // k from the top wraps to the last row, Web Remote Access.
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    let buf = tui::render(&mut app, 120, 40);
    let lan = must_row(&buf, WEB_LAN);
    assert_eq!(span(&buf, lan, VALUE_X, BAR_END_X + 1), "◂ THIS COMPUTER ▸");
    assert!(is_cursor(&buf, LABEL_X, lan));
}

#[test]
fn footnote_shows_the_config_file_in_its_own_case() {
    common::isolate();
    let mut app = settings_app("footnote");
    let buf = tui::render(&mut app, 120, 40);
    let foot = label_row(&buf) + 2;
    let text = span(&buf, foot, LABEL_X, OPTION_X - 4);
    assert!(text.ends_with("Config.JSON"), "{text:?}");
    assert!(!dump(&buf).contains("CONFIG.JSON"));
    assert_eq!(tui::cell_style(&buf, LABEL_X, foot).fg, Some(P.dim));
    // Too long for the row, the start of the path gives way.
    if app.settings_mgr.path.display().to_string().chars().count() > 70 {
        assert!(text.starts_with('…'), "{text:?}");
    }
}

// ---------- the option panel ----------

#[test]
fn option_panel_at_120_lists_the_choices_and_describes_the_setting() {
    common::isolate();
    let mut app = settings_app("options");
    app.settings_cursor = QUEUE_MODE;
    let buf = tui::render(&mut app, 120, 40);

    let top = label_row(&buf);
    assert_eq!(span(&buf, top, OPTION_X, 118), "QUEUE PLAYBACK MODE");
    assert!(bold(&buf, OPTION_X, top));
    assert_eq!(span(&buf, top + 1, OPTION_X, 120), format!("{} ▪", "─".repeat(38)));
    assert_eq!(span(&buf, top + 2, OPTION_X, 118), "04 / 12", "display position");
    assert_eq!(tui::cell_style(&buf, OPTION_X, top + 2).fg, Some(P.dim));

    let first = top + 4;
    assert_eq!(span(&buf, first, OPTION_X, CHOICE_X), "●");
    assert_eq!(span(&buf, first, CHOICE_X, 118), "CONTINUOUS (LOOP)");
    assert!(bold(&buf, CHOICE_X, first));
    assert_eq!(tui::cell_style(&buf, CHOICE_X, first).fg, Some(P.ink));
    assert_eq!(span(&buf, first + 1, OPTION_X, CHOICE_X), "○");
    assert_eq!(tui::cell_style(&buf, OPTION_X, first + 1).fg, Some(P.mid));
    assert_eq!(span(&buf, first + 1, CHOICE_X, 118), "STOP AT END");
    assert_eq!(tui::cell_style(&buf, CHOICE_X, first + 1).fg, Some(P.mid));
    assert_eq!(span(&buf, first + 2, CHOICE_X, 118), "REPEAT TRACK");

    // The description, after a blank row, in MID.
    assert_eq!(span(&buf, first + 3, OPTION_X, 120), "");
    let words: Vec<&str> = SETTING_DEFINITIONS[QUEUE_MODE].description.split(' ').collect();
    let (x, y) = text_from(&buf, words[0], OPTION_X).expect("description");
    assert_eq!((x, y), (OPTION_X, first + 4));
    assert_eq!(tui::cell_style(&buf, x, y).fg, Some(P.mid));

    // Right cycles the setting; the ● follows.
    press(&mut app, KeyCode::Right);
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(span(&buf, first, OPTION_X, CHOICE_X), "○");
    assert_eq!(span(&buf, first + 1, OPTION_X, CHOICE_X), "●");
    assert_eq!(span(&buf, first + 1, CHOICE_X, 118), "STOP AT END");
    assert_eq!(
        span(&buf, must_row(&buf, QUEUE_MODE), VALUE_X, BAR_END_X + 1),
        "◂ STOP AT END ▸"
    );
}

#[test]
fn every_description_shows_whole_in_the_option_panel() {
    common::isolate();
    let mut app = settings_app("descriptions");
    for (index, def) in SETTING_DEFINITIONS.iter().enumerate() {
        let description = def.description;
        assert!(!description.is_empty());
        app.settings_cursor = index;
        let buf = tui::render(&mut app, 120, 40);
        let shown: String = (0..buf.area.height)
            .map(|y| span(&buf, y, OPTION_X, 120))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            shown.contains(description),
            "{}: {description:?} not whole in\n{}",
            def.label,
            dump(&buf)
        );
    }
}

#[test]
fn startup_home_choices_come_from_the_bookmarks_in_memory() {
    common::isolate();
    let mut app = settings_app("home_choices");
    // Written after startup: drawing must not read it.
    fs::write(
        quickstream::paths::get_bookmarks_path(),
        r#"["/qs-tab-from-file/bookmark"]"#,
    )
    .unwrap();
    app.bookmarks = vec!["/qs-tab-from-memory/Bookmark".to_string()];
    app.settings_cursor = HOME_DIR;
    let buf = tui::render(&mut app, 120, 40);
    let text = dump(&buf);
    assert!(text.contains("/qs-tab-from-memory/Bookmark"), "{text}");
    assert!(
        !text.contains("/qs-tab-from-file"),
        "bookmarks.json must not be read:\n{text}"
    );
    let (x, y) = text_from(&buf, "LAUNCH DIRECTORY", CHOICE_X).expect("launch directory choice");
    assert_eq!(x, CHOICE_X);
    assert_eq!(span(&buf, y, OPTION_X, CHOICE_X), "●");
    let (_, y) = text_from(&buf, "/qs-tab-from-memory/Bookmark", CHOICE_X).unwrap();
    assert_eq!(span(&buf, y, OPTION_X, CHOICE_X), "○");
}

#[test]
fn under_110_columns_there_is_no_option_panel_and_the_position_joins_the_footnote() {
    common::isolate();
    let mut app = settings_app("medium");
    app.settings_cursor = QUEUE_MODE;
    let buf = tui::render(&mut app, 100, 30);
    let text = dump(&buf);
    assert!(!text.contains('●') && !text.contains('○'), "{text}");
    let foot = label_row(&buf) + 2;
    let line = span(&buf, foot, 0, 100);
    assert!(line.contains("Config.JSON") && line.ends_with("04 / 12"), "{line:?}");
    let queue = must_row(&buf, QUEUE_MODE);
    assert_eq!(span(&buf, queue, VALUE_X, 100), "◂ CONTINUOUS (LOOP) ▸");
    // The bar spans the whole list: 100 - 2 gutters, one column past the text on each side.
    assert!(is_cursor(&buf, 1, queue) && is_cursor(&buf, 96, queue) && !is_cursor(&buf, 97, queue));
}

// ---------- web rows ----------

#[test]
fn web_address_keeps_its_case_in_the_option_panel_and_the_footnote() {
    common::isolate();
    let mut app = settings_app("web");
    let port = start_loopback_web(&mut app);
    let url = format!("http://127.0.0.1:{port}");
    // (Turning it on put the address in the status line too.)
    app.status_message.clear();
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        span(&buf, must_row(&buf, WEB_SERVER), VALUE_X, BAR_END_X + 1),
        format!("◂ ON  :{port} ▸")
    );
    let (x, y) = text_from(&buf, &url, OPTION_X).expect("web address");
    assert!(x >= OPTION_X);
    assert_eq!(tui::cell_style(&buf, x, y).fg, Some(P.dim));
    assert!(!dump(&buf).contains("HTTP://"));

    // Under 110 columns the address takes the footnote while a web row is selected.
    let buf = tui::render(&mut app, 100, 30);
    let foot = label_row(&buf) + 2;
    let line = span(&buf, foot, 0, 100);
    assert!(line.contains(&url), "{line:?}");
    app.settings_cursor = WEB_LAN;
    let buf = tui::render(&mut app, 100, 30);
    assert!(span(&buf, label_row(&buf) + 2, 0, 100).contains(&url));
    app.settings_cursor = QUEUE_MODE;
    let buf = tui::render(&mut app, 100, 30);
    assert!(!dump(&buf).contains("http://"));
}

#[test]
fn web_rows_show_the_port_the_server_bound_not_the_configured_one() {
    common::isolate();
    let mut app = settings_app("bound_port");
    // Another program holds the configured port, so the server moves to another one.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let configured = held.local_addr().unwrap().port();
    app.settings_mgr.settings.web_server_port = configured;
    app.settings_cursor = WEB_SERVER;
    press(&mut app, KeyCode::Right);
    let bound = app.web_server.as_ref().expect("web server started").port;
    assert_ne!(bound, configured);
    let url = format!("http://127.0.0.1:{bound}");
    assert!(app.status_message.contains(&url), "{}", app.status_message);

    let buf = tui::render(&mut app, 120, 40);
    let header = tui::row(&buf, row_of(&buf, "QUICKSTREAM"));
    assert!(header.contains(&format!("WEB :{bound}")), "{header}");
    assert_eq!(
        span(&buf, must_row(&buf, WEB_SERVER), VALUE_X, BAR_END_X + 1),
        format!("◂ ON  :{bound} ▸")
    );
    assert!(
        text_from(&buf, &format!("●   ON  :{bound}"), OPTION_X).is_some(),
        "\n{}",
        dump(&buf)
    );
    assert!(text_from(&buf, &url, OPTION_X).is_some(), "\n{}", dump(&buf));
    assert!(!dump(&buf).contains(&format!(":{configured}")), "\n{}", dump(&buf));

    let buf = tui::render(&mut app, 100, 30);
    let line = span(&buf, label_row(&buf) + 2, 0, 100);
    assert!(line.starts_with(&format!("{url}  ·  ")), "{line:?}");

    // Turned off: the server stops and no port is shown.
    press(&mut app, KeyCode::Left);
    assert!(app.web_server.is_none());
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        span(&buf, must_row(&buf, WEB_SERVER), VALUE_X, BAR_END_X + 1),
        "◂ OFF ▸"
    );
    assert!(text_from(&buf, "○   ON", OPTION_X).is_some(), "\n{}", dump(&buf));
    assert!(!dump(&buf).contains(&format!(":{bound}")), "\n{}", dump(&buf));
    drop(held);
}

#[test]
fn web_server_started_at_launch_shows_its_bound_port() {
    let root = common::isolate();
    let dir = root.join("settings_tab_launch_port");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let configured = held.local_addr().unwrap().port();
    let mut settings = SettingsManager::with_path(dir.join("config.json"));
    settings.settings.web_server_enabled = true;
    settings.settings.web_server_port = configured;
    let mut app = AppUI::new(Some(dir), settings);
    app.player = Box::new(FakePlayer::default());
    app.active_tab = 3;
    app.settings_cursor = WEB_SERVER;
    let bound = app.web_server.as_ref().expect("web server started at launch").port;
    assert_ne!(bound, configured);

    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        span(&buf, must_row(&buf, WEB_SERVER), VALUE_X, BAR_END_X + 1),
        format!("◂ ON  :{bound} ▸")
    );
    assert!(text_from(&buf, &format!("http://127.0.0.1:{bound}"), OPTION_X).is_some());
    if let Some(handle) = app.web_server.take() {
        handle.stop();
    }
    drop(held);
}

// ---------- the startup home editor ----------

#[test]
fn startup_home_editor_takes_typed_text_inline_including_q() {
    common::isolate();
    let mut app = settings_app("editor");
    app.settings_cursor = HOME_DIR;
    assert!(!press(&mut app, KeyCode::Enter));
    assert_eq!(app.editing_home_dir.as_deref(), Some(""));
    type_text(&mut app, "~/quiet Music");
    assert_eq!(app.editing_home_dir.as_deref(), Some("~/quiet Music"));

    let buf = tui::render(&mut app, 120, 40);
    let row = must_row(&buf, HOME_DIR);
    assert_eq!(span(&buf, row, VALUE_X, BAR_END_X + 1), "~/quiet Music█");
    assert!((1..=BAR_END_X).all(|x| is_cursor(&buf, x, row)));
    // The option panel says what the field takes.
    let panel: String = (0..buf.area.height)
        .map(|y| span(&buf, y, OPTION_X, 120))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(panel.contains("launch folder"), "{panel}");
    assert!(!panel.contains('●'), "no choices while typing");
    // The key row shows the editor's keys, and no ? KEYS (? is text here).
    assert_eq!(hints(&app), pairs(&[("ENTER", "SAVE"), ("ESC", "CANCEL")]));
    let keys = row_of(&buf, " ENTER  SAVE");
    assert!(!tui::row(&buf, keys).contains("KEYS"));

    // A long path shows its end, still with the cursor.
    type_text(&mut app, &"/deeper".repeat(12));
    for (w, h) in [(120, 40), (100, 30), (80, 24)] {
        let buf = tui::render(&mut app, w, h);
        let row = must_row(&buf, HOME_DIR);
        let value = span(&buf, row, VALUE_X, w);
        assert!(
            value.starts_with('…') && value.ends_with("/deeper█"),
            "{w}x{h}: {value:?}"
        );
    }

    // Esc closes it without saving.
    press(&mut app, KeyCode::Esc);
    assert!(app.editing_home_dir.is_none());
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        span(&buf, must_row(&buf, HOME_DIR), VALUE_X, BAR_END_X + 1),
        "◂ LAUNCH DIRECTORY ▸"
    );

    // Enter saves what was typed.
    press(&mut app, KeyCode::Enter);
    let target = common::isolate().join("qs-home-target");
    type_text(&mut app, &target.display().to_string());
    press(&mut app, KeyCode::Enter);
    assert!(app.editing_home_dir.is_none());
    assert_eq!(
        app.settings_mgr.settings.home_dir.as_deref(),
        Some(target.to_str().unwrap())
    );
}

// ---------- key row ----------

#[test]
fn settings_key_row_matches_the_handlers() {
    common::isolate();
    let mut app = settings_app("keys");
    app.settings_cursor = QUEUE_MODE;
    assert_eq!(
        hints(&app),
        pairs(&[("J K", "SELECT"), ("← →", "CHANGE"), ("ENTER", "CHANGE"), ("?", "KEYS")])
    );
    // Enter cycles forward on every row but the startup home.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.settings_mgr.settings.queue_mode, "stop");
    let buf = tui::render(&mut app, 120, 40);
    row_of(&buf, " J K  SELECT    ");

    // On the startup home Enter opens the editor and C clears the path.
    app.settings_cursor = HOME_DIR;
    assert_eq!(
        hints(&app),
        pairs(&[
            ("J K", "SELECT"),
            ("← →", "CHANGE"),
            ("ENTER", "EDIT"),
            ("C", "CLEAR"),
            ("?", "KEYS")
        ])
    );
    app.settings_mgr.settings.home_dir = Some("/qs-clear-me".to_string());
    press(&mut app, KeyCode::Char('c'));
    assert_eq!(app.settings_mgr.settings.home_dir, None);
    press(&mut app, KeyCode::Enter);
    assert!(app.editing_home_dir.is_some());
}

// ---------- sizes ----------

#[test]
fn settings_render_at_120x40_100x30_and_80x24() {
    common::isolate();
    let mut app = settings_app("sizes");
    for (w, h) in [(120, 40), (100, 30), (80, 24)] {
        app.settings_cursor = SETTING_DISPLAY_ORDER[0];
        for step in 0..SETTING_DISPLAY_ORDER.len() {
            let index = app.settings_cursor;
            let buf = tui::render(&mut app, w, h);
            let row = setting_row(&buf, index)
                .unwrap_or_else(|| panic!("{w}x{h}: cursor row {index} not shown:\n{}", dump(&buf)));
            assert!(is_cursor(&buf, LABEL_X, row), "{w}x{h} step {step}");
            assert!(span(&buf, row, VALUE_X, w).starts_with('◂'), "{w}x{h} step {step}");
            press(&mut app, KeyCode::Char('j'));
        }
    }
    // 120x40 shows every row, the group headings and the blank rows between groups.
    let buf = tui::render(&mut app, 120, 40);
    for index in 0..SETTING_DEFINITIONS.len() {
        must_row(&buf, index);
    }
    let library = row_of(&buf, "LIBRARY ");
    assert_eq!(span(&buf, library - 1, 0, 120), "");
    // 100x30 is a row short of that: the blank rows go first, the headings stay.
    app.settings_cursor = SETTING_DISPLAY_ORDER[0];
    let buf = tui::render(&mut app, 100, 30);
    let library = (0..30)
        .find(|&y| span(&buf, y, LABEL_X, VALUE_X) == "LIBRARY")
        .expect("LIBRARY");
    assert_eq!(span(&buf, library - 1, LABEL_X, VALUE_X), "AUDIO TRANSCODING");
    assert!((0..30).any(|y| span(&buf, y, LABEL_X, VALUE_X) == "PLAYBACK"));
    // 80x24 is compact: the position moves to the label row, the path drops.
    app.settings_cursor = QUEUE_MODE;
    let buf = tui::render(&mut app, 80, 24);
    let top = label_row(&buf);
    assert!(
        tui::row(&buf, top).trim_end().ends_with("04 / 12"),
        "{}",
        tui::row(&buf, top)
    );
    assert!(!dump(&buf).contains("Config.JSON"));
    assert!(!dump(&buf).contains('●'));
}

#[test]
fn the_first_row_of_a_group_brings_its_heading_into_view() {
    common::isolate();
    let mut app = settings_app("headings");
    // Scroll down to the last row, then back up to the first LIBRARY row.
    app.settings_cursor = SETTING_DISPLAY_ORDER[SETTING_DISPLAY_ORDER.len() - 1];
    tui::render(&mut app, 80, 24);
    app.settings_cursor = 0; // File List Display, the first LIBRARY row
    let buf = tui::render(&mut app, 80, 24);
    let row = must_row(&buf, 0);
    assert_eq!(span(&buf, row - 1, LABEL_X, VALUE_X), "LIBRARY", "\n{}", dump(&buf));
}

#[test]
fn a_list_scrolled_on_a_short_terminal_shows_whole_again_when_it_fits() {
    common::isolate();
    let mut app = settings_app("refit");
    app.settings_cursor = SETTING_DISPLAY_ORDER[SETTING_DISPLAY_ORDER.len() - 1];
    tui::render(&mut app, 80, 24);
    app.settings_cursor = HOME_DIR;
    let buf = tui::render(&mut app, 120, 40);
    for index in 0..SETTING_DEFINITIONS.len() {
        must_row(&buf, index);
    }
}

#[test]
fn narrow_settings_tab_does_not_panic_and_keeps_the_cursor_row_visible() {
    common::isolate();
    let mut app = settings_app("narrow");
    let sizes = [
        (70, 24),
        (60, 20),
        (50, 16),
        (40, 12),
        (30, 10),
        (20, 8),
        (10, 5),
        (3, 2),
        (1, 1),
    ];
    for editing in [false, true] {
        for &index in &SETTING_DISPLAY_ORDER {
            app.settings_cursor = index;
            app.editing_home_dir = (editing && index == HOME_DIR).then(|| "~/Music/quiet".to_string());
            for (w, h) in sizes {
                let buf = tui::render(&mut app, w, h);
                if w >= 30 && h >= 10 {
                    // The cursor bar is on screen, on a row that starts like the setting's label.
                    let label = kit::upper(SETTING_DEFINITIONS[index].label);
                    let shown = (0..h).any(|y| {
                        let text = span(&buf, y, LABEL_X, w);
                        let start: String = text.chars().take(4).collect();
                        is_cursor(&buf, LABEL_X, y) && label.starts_with(&start)
                    });
                    assert!(shown, "{w}x{h} {index}:\n{}", dump(&buf));
                }
            }
        }
    }
    app.editing_home_dir = None;
}
