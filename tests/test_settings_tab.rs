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
use quickstream::analysis::bpm::FfmpegConfig;
use quickstream::analysis::worker::AnalysisWorker;
use quickstream::analysis::{AnalysisDatabase, TrackFeatures};
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
    // The LIBRARY group ends with the read-only TRACK ANALYSIS row (#30).
    let web = want.iter().position(|row| row == "WEB").unwrap();
    want.insert(web - 1, "TRACK ANALYSIS".to_string());
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

#[test]
fn web_address_shows_at_every_width_while_a_web_row_is_selected() {
    common::isolate();
    let mut app = settings_app("web_every_width");
    let port = start_loopback_web(&mut app);
    let url = format!("http://127.0.0.1:{port}");
    app.status_message.clear();
    // Compact (under 90 columns, or under 24 rows) and wide-but-short layouts included.
    for (w, h) in [
        (120, 40),
        (120, 20),
        (100, 30),
        (100, 20),
        (85, 30),
        (80, 24),
        (60, 24),
        (40, 24),
    ] {
        for cursor in [WEB_SERVER, WEB_LAN] {
            app.settings_cursor = cursor;
            let buf = tui::render(&mut app, w, h);
            let (x, y) = tui::find_text(&buf, &url)
                .unwrap_or_else(|| panic!("{w}x{h}, row {cursor}: no {url}:\n{}", dump(&buf)));
            assert_eq!(tui::cell_style(&buf, x, y).fg, Some(P.dim), "{w}x{h}");
        }
        app.settings_cursor = QUEUE_MODE;
        let buf = tui::render(&mut app, w, h);
        assert!(!dump(&buf).contains("http://"), "{w}x{h}:\n{}", dump(&buf));
    }
}

#[test]
fn compact_pairing_link_wraps_and_is_cut_from_the_start_only_when_it_cannot_fit() {
    common::isolate();
    let mut app = settings_app("web_pairing_compact");
    let port = start_loopback_web(&mut app);
    // Network access on in the settings only (the server stays on loopback).
    app.settings_mgr.settings.web_server_lan = true;
    app.settings_mgr.set_lan_ip_resolver(|| "192.0.2.60".to_string());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while app.settings_mgr.web_url(Some(port)).is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let link = app.settings_mgr.web_url(Some(port)).expect("pairing link");
    assert!(link.contains("?token="), "{link}");
    app.status_message.clear();
    app.settings_cursor = WEB_SERVER;
    let token = quickstream::web_access::web_token();

    // 80 columns: one row, whole.
    let buf = tui::render(&mut app, 80, 24);
    assert!(tui::find_text(&buf, &link).is_some(), "{link}\n{}", dump(&buf));

    // Narrower than the link: two rows, whole once joined.
    let buf = tui::render(&mut app, 50, 24);
    let rows = tui::rows(&buf);
    let at = rows
        .iter()
        .position(|r| r.trim_start().starts_with("http://192.0.2.60"))
        .unwrap_or_else(|| panic!("no link row:\n{}", dump(&buf)));
    let joined = format!("{}{}", rows[at].trim(), rows[at + 1].trim());
    assert_eq!(joined, link, "\n{}", dump(&buf));

    // Wider than two rows: the start is cut with `…`, the token's end still shows.
    let buf = tui::render(&mut app, 30, 24);
    let text = dump(&buf);
    let rows = tui::rows(&buf);
    let at = rows
        .iter()
        .position(|r| r.trim_start().starts_with('…'))
        .unwrap_or_else(|| panic!("no cut link row:\n{text}"));
    let joined = format!("{}{}", rows[at].trim(), rows[at + 1].trim());
    assert!(joined.ends_with(token), "{joined}\n{text}");
    assert!(link.ends_with(joined.trim_start_matches('…')), "{joined}");
}

#[test]
fn a_long_startup_home_keeps_its_last_folder() {
    common::isolate();
    let mut app = settings_app("long_home");
    let path = "/Volumes/Music/Classical/Deutsche Grammophon/Berliner Philharmoniker/Herbert von Karajan";
    app.settings_mgr.settings.home_dir = Some(path.to_string());
    for (w, h) in [(120, 40), (100, 30), (80, 24), (60, 24)] {
        // Selected: `◂ …/Herbert von Karajan ▸`, its start cut.
        app.settings_cursor = HOME_DIR;
        let buf = tui::render(&mut app, w, h);
        let row = must_row(&buf, HOME_DIR);
        let value = tui::row(&buf, row);
        // (At 60 columns only the end of the last folder fits.)
        let last = if w >= 80 {
            "Herbert von Karajan ▸"
        } else {
            "Karajan ▸"
        };
        assert!(
            value.contains("◂ …") && value.trim_end().ends_with(last),
            "{w}x{h}: {value:?}"
        );
        // Not selected: cut from the start too.
        app.settings_cursor = QUEUE_MODE;
        let buf = tui::render(&mut app, w, h);
        let value = tui::row(&buf, must_row(&buf, HOME_DIR));
        assert!(
            value.contains('…') && value.trim_end().ends_with("Herbert von Karajan"),
            "{w}x{h}: {value:?}"
        );
    }
    // `LAUNCH DIRECTORY` is no path: too wide, its end is cut.
    app.settings_mgr.settings.home_dir = None;
    app.settings_cursor = QUEUE_MODE;
    let buf = tui::render(&mut app, 30, 24);
    let value = tui::row(&buf, row_of(&buf, "STARTUP"));
    assert!(value.contains("LAUNCH") && !value.contains("…H DIRECTORY"), "{value:?}");
}

/// The DIM text of row `y` between columns `from` and `to` (a link's cells; descriptions,
/// choices and values are MID or INK).
fn dim_text(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    (from..to.min(buf.area.width))
        .filter(|&x| tui::cell_style(buf, x, y).fg == Some(P.dim))
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect::<String>()
        .trim()
        .to_string()
}

/// `link` as drawn between columns `from` and `to`: its first row (starting `http://`, or `…`
/// when its start is cut) joined with the DIM rows under it that carry on the link.
fn shown_link(buf: &Buffer, from: u16, to: u16, link: &str) -> Option<String> {
    let part_of = |text: &str| !text.is_empty() && link.contains(text.trim_start_matches('…'));
    (0..buf.area.height).find_map(|y| {
        let first = dim_text(buf, y, from, to);
        if !(first.starts_with("http://") || first.starts_with('…')) || !part_of(&first) {
            return None;
        }
        let mut joined = first;
        for next in y + 1..buf.area.height {
            let more = format!("{joined}{}", dim_text(buf, next, from, to));
            if more.len() == joined.len() || !part_of(&more) {
                break;
            }
            joined = more;
        }
        Some(joined)
    })
}

/// Network access on in the settings only (the server stays on loopback), with the LAN address
/// `lan`; returns the pairing link.
fn pairing_link(app: &mut AppUI, port: u16, lan: &'static str) -> String {
    app.settings_mgr.settings.web_server_lan = true;
    app.settings_mgr.set_lan_ip_resolver(move || lan.to_string());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while app.settings_mgr.web_url(Some(port)).is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let link = app.settings_mgr.web_url(Some(port)).expect("pairing link");
    assert!(link.contains("?token="), "{link}");
    link
}

/// Short terminals from 110 columns, where the option panel is drawn: the address is never
/// left out, and never shown cut at its end (a pairing link missing part of its token). From 8
/// rows tall the option panel has the two rows a pairing link takes.
#[test]
fn web_address_shows_whole_on_short_terminals_with_the_option_panel() {
    common::isolate();
    let mut app = settings_app("web_short_full");
    let port = start_loopback_web(&mut app);
    app.status_message.clear();
    let loopback = format!("http://127.0.0.1:{port}");
    let mut links = vec![loopback];
    links.push(pairing_link(&mut app, port, "192.168.100.200"));
    let token = quickstream::web_access::web_token();
    for (n, link) in links.iter().enumerate() {
        app.settings_mgr.settings.web_server_lan = n == 1;
        for w in [110u16, 120, 140] {
            for h in 8u16..=23 {
                for cursor in [WEB_SERVER, WEB_LAN] {
                    app.settings_cursor = cursor;
                    let buf = tui::render(&mut app, w, h);
                    let text = dump(&buf);
                    // The list, or the option panel 40 columns from the right gutter.
                    let panel_x = w - 2 - 40;
                    let shown = shown_link(&buf, panel_x, w, link)
                        .or_else(|| shown_link(&buf, 0, panel_x, link))
                        .unwrap_or_else(|| panic!("{w}x{h}, row {cursor}: no {link}:\n{text}"));
                    assert_eq!(&shown, link, "{w}x{h}, row {cursor}:\n{text}");
                    assert!(shown.ends_with(&format!(":{port}")) || shown.ends_with(token));
                }
            }
        }
    }
}

fn text_width_of(text: &str) -> usize {
    text.chars().count()
}

/// Under 110 columns on short terminals the address under the list takes the two rows it needs,
/// so its host is not cut while two rows would hold it.
#[test]
fn compact_pairing_link_takes_two_rows_on_short_terminals() {
    common::isolate();
    let mut app = settings_app("web_short_compact");
    let port = start_loopback_web(&mut app);
    app.status_message.clear();
    let link = pairing_link(&mut app, port, "192.168.100.200");
    for w in [40u16, 50, 60, 73] {
        for h in 9u16..=13 {
            for cursor in [WEB_SERVER, WEB_LAN] {
                app.settings_cursor = cursor;
                let buf = tui::render(&mut app, w, h);
                let text = dump(&buf);
                let shown =
                    shown_link(&buf, 0, w, &link).unwrap_or_else(|| panic!("{w}x{h}, row {cursor}: no link:\n{text}"));
                // Two rows of the list's text width hold it whole.
                if text_width_of(&link) <= 2 * usize::from(w - 4 - 2) {
                    assert_eq!(shown, link, "{w}x{h}, row {cursor}:\n{text}");
                }
                // The cursor row stays on screen (its label may be cut at these widths).
                let label = kit::upper(SETTING_DEFINITIONS[cursor].label);
                let start: String = label.chars().take(6).collect();
                let on_screen =
                    (0..h).any(|y| is_cursor(&buf, LABEL_X, y) && span(&buf, y, LABEL_X, w).starts_with(&start));
                assert!(on_screen, "{w}x{h}, row {cursor}:\n{text}");
            }
        }
    }
}

// ---------- track analysis (#30) ----------

const ANALYSIS: &str = "TRACK ANALYSIS";

/// A worker on its own database in `dir` whose counters read `analysed` analysed, `up_to_date`
/// skipped (their rows match the files) and `pending` waiting, the first of those in flight: a
/// fake ffmpeg answers the others at once with 10 s of silence (a real result, stored without a
/// BPM) and sleeps on files named `slow-*` until the worker shuts down. The files are not audio:
/// with no tags to read, every one goes to ffmpeg.
#[cfg(unix)]
fn analysis_worker(dir: &std::path::Path, analysed: usize, up_to_date: usize, pending: usize) -> AnalysisWorker {
    use std::time::{Duration, UNIX_EPOCH};

    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let script = dir.join("fake-ffmpeg");
    common::fake_exec::write_script(
        &script,
        "#!/bin/sh\ncase \"$*\" in *slow-*) exec sleep 60;; esac\nhead -c 220500 /dev/zero\nexit 0\n",
    );
    let file = |name: String| {
        let path = dir.join(name);
        fs::write(&path, b"not audio").unwrap();
        path
    };

    let db = AnalysisDatabase::open_at(&dir.join("track_analysis.db")).unwrap();
    let skipped: Vec<_> = (0..up_to_date).map(|i| file(format!("done-{i}.flac"))).collect();
    for path in &skipped {
        let md = fs::metadata(path).unwrap();
        db.upsert_track(&TrackFeatures {
            filepath: path.clone(),
            mtime: md.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            file_size: md.len(),
            ..Default::default()
        })
        .unwrap();
    }
    let ffmpeg = FfmpegConfig {
        binary: script,
        ..FfmpegConfig::default()
    };
    let worker = AnalysisWorker::spawn_with_ffmpeg(db, 1, ffmpeg);
    worker.enqueue(
        skipped
            .into_iter()
            .chain((0..analysed).map(|i| file(format!("new-{i}.flac")))),
    );
    assert!(worker.wait_until_idle(Duration::from_secs(20)), "worker did not finish");
    worker.enqueue((0..pending).map(|i| file(format!("slow-{i}.flac"))));
    let progress = worker.progress();
    assert_eq!(
        (progress.analyzed, progress.skipped, progress.failed, progress.pending),
        (analysed, up_to_date, 0, pending)
    );
    worker
}

/// The value of the TRACK ANALYSIS row and the row, if it is on screen.
fn analysis_row(buf: &Buffer) -> Option<(u16, String)> {
    let y = (0..buf.area.height).find(|&y| span(buf, y, LABEL_X, VALUE_X) == ANALYSIS)?;
    Some((y, span(buf, y, VALUE_X, buf.area.width)))
}

#[cfg(unix)]
#[test]
fn track_analysis_row_shows_the_workers_counters_at_every_size() {
    common::isolate();
    let mut app = settings_app("analysis_sizes");
    let dir = std::env::temp_dir().join(format!("qs_settings_analysis_{}", std::process::id()));
    app.analysis_worker = analysis_worker(&dir, 2, 1, 3);

    let last_library = SETTING_DISPLAY_ORDER
        .iter()
        .rev()
        .find(|&&i| SETTING_DEFINITIONS[i].group == SettingGroup::Library)
        .copied()
        .unwrap();
    // Every size from mockup 2e down to a short terminal (3h). Where the list scrolls, moving
    // down to the LIBRARY group's last setting brings the row under it into view.
    for (w, h) in [(120, 40), (100, 30), (80, 24), (80, 20)] {
        app.settings_cursor = SETTING_DISPLAY_ORDER[0];
        tui::render(&mut app, w, h);
        while app.settings_cursor != last_library {
            press(&mut app, KeyCode::Char('j'));
            tui::render(&mut app, w, h);
        }
        let buf = tui::render(&mut app, w, h);
        let (y, value) = analysis_row(&buf).unwrap_or_else(|| panic!("{w}x{h}: no {ANALYSIS} row:\n{}", dump(&buf)));
        assert_eq!(
            value,
            "3 PENDING  ·  2 ANALYSED  ·  1 SKIPPED",
            "{w}x{h}:\n{}",
            dump(&buf)
        );
        // The LIBRARY group's last row, read-only: label and value MID, never the cursor bar.
        assert_eq!(must_row(&buf, last_library) + 1, y, "{w}x{h}:\n{}", dump(&buf));
        assert!(is_cursor(&buf, LABEL_X, y - 1), "{w}x{h}: the cursor stays on its row");
        assert_eq!(tui::cell_style(&buf, LABEL_X, y).fg, Some(P.mid), "{w}x{h}");
        assert_eq!(tui::cell_style(&buf, VALUE_X, y).fg, Some(P.mid), "{w}x{h}");
        assert!(!is_cursor(&buf, LABEL_X, y), "{w}x{h}");
    }

    // The cursor never lands on it: down from the LIBRARY group's last setting is the WEB group.
    let order = &SETTING_DISPLAY_ORDER;
    let at = order
        .iter()
        .rposition(|&i| SETTING_DEFINITIONS[i].group == SettingGroup::Library)
        .unwrap();
    app.settings_cursor = order[at];
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.settings_cursor, order[at + 1]);
    assert_eq!(SETTING_DEFINITIONS[app.settings_cursor].group, SettingGroup::Web);

    drop(app);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn track_analysis_row_follows_the_worker_and_leaves_out_what_is_zero() {
    common::isolate();
    let mut app = settings_app("analysis_live");
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        analysis_row(&buf).map(|r| r.1).as_deref(),
        Some("IDLE"),
        "nothing analysed yet:\n{}",
        dump(&buf)
    );

    let dir = std::env::temp_dir().join(format!("qs_settings_analysis_live_{}", std::process::id()));
    app.analysis_worker = analysis_worker(&dir, 4, 0, 0);
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(analysis_row(&buf).map(|r| r.1).as_deref(), Some("4 ANALYSED"));
    // Tracks queued later show at once: the counters are read on every frame (atomics).
    let late = dir.join("slow-late.flac");
    fs::write(&late, b"not audio").unwrap();
    app.analysis_worker.enqueue([late]);
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(
        analysis_row(&buf).map(|r| r.1).as_deref(),
        Some("1 PENDING  ·  4 ANALYSED")
    );

    drop(app);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn track_analysis_value_is_cut_from_its_end_keeping_pending() {
    common::isolate();
    let mut app = settings_app("analysis_narrow");
    let dir = std::env::temp_dir().join(format!("qs_settings_analysis_narrow_{}", std::process::id()));
    app.analysis_worker = analysis_worker(&dir, 2, 1, 3);
    app.settings_cursor = SETTING_DISPLAY_ORDER[0];
    // 60 columns leave the value 22: pending comes first, so it stays.
    let buf = tui::render(&mut app, 60, 30);
    let (_, value) = analysis_row(&buf).unwrap_or_else(|| panic!("no {ANALYSIS} row:\n{}", dump(&buf)));
    assert_eq!(value, "3 PENDING  ·  2 ANALY…", "\n{}", dump(&buf));

    drop(app);
    let _ = fs::remove_dir_all(&dir);
}
