//! Tab 3 Devices in the Telemetry style (mockup 2d at 120×40, width rules 3h): the OUTPUT
//! DEVICES table (Local Audio and the discovered renderers), the active output, the cursor, the
//! `FOUND · SSDP` footnote, the SCANNING note, the DEVICE detail panel from 110 columns, and
//! narrow terminals. The renderers are made up in the test (no network).

mod common;

use std::fs;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use common::fake_player::FakePlayer;
use common::tui;
use quickstream::discovery::DLNADevice;
use quickstream::settings::SettingsManager;
use quickstream::ui::theme::Palette;
use quickstream::ui::{kit, theme, AppUI};

const P: Palette = Palette::TRUECOLOR;

/// At 120 columns (2d): the table's text starts at the gutter, the name, type and address
/// columns follow a 4-column marker column, and the detail panel starts 4 columns after the
/// 72-column OUTPUT DEVICES panel.
const MARK_X: u16 = 2;
const NAME_X: u16 = 6;
const TYPE_X: u16 = 36;
const ADDRESS_X: u16 = 46;
const DETAIL_X: u16 = 78;
const DETAIL_VALUE_X: u16 = 90;

fn renderer(name: &str, host: &str, port: u16, maker: &str, model: &str, udn: &str) -> DLNADevice {
    DLNADevice {
        friendly_name: name.to_string(),
        location: format!("http://{host}:{port}/description.xml"),
        host: host.to_string(),
        port,
        manufacturer: maker.to_string(),
        model_name: model.to_string(),
        av_transport_control_url: format!("http://{host}:{port}/AVTransport/control"),
        rendering_control_url: format!("http://{host}:{port}/RenderingControl/control"),
        udn: udn.to_string(),
    }
}

/// The three renderers of 2d.
fn mockup_renderers() -> Vec<DLNADevice> {
    vec![
        renderer(
            "Primare I15 Prisma",
            "192.168.1.145",
            8080,
            "Primare",
            "I15",
            "uuid:5f9e0d2a-77b1-4c3e-9a10-2b6e8f04c41a",
        ),
        renderer(
            "Living Room",
            "192.168.1.60",
            1400,
            "Sonos",
            "Play:5",
            "uuid:RINCON_000E58",
        ),
        renderer("Denon AVR-X2800H", "192.168.1.72", 8080, "", "", "uuid:denon-x2800h"),
    ]
}

/// An app on tab 3 with `devices` discovered, a fake player at volume 65 reporting mpv, and
/// discovery idle.
fn devices_app(tag: &str, devices: Vec<DLNADevice>) -> AppUI {
    let dir = std::env::temp_dir().join(format!("quickstream_devices_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir), settings);
    app.player = Box::new(FakePlayer {
        volume: 65,
        backend: Some("mpv"),
        ..FakePlayer::default()
    });
    *app.device_manager.devices.lock().unwrap() = devices;
    app.active_tab = 2;
    app
}

fn set_scanning(app: &AppUI, scanning: bool) {
    *app.device_manager.is_scanning.lock().unwrap() = scanning;
}

fn press(app: &mut AppUI, code: KeyCode) {
    let _ = app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn row_of(buf: &Buffer, text: &str) -> u16 {
    tui::find_row(buf, text).unwrap_or_else(|| panic!("no row with {text:?}:\n{}", tui::rows(buf).join("\n")))
}

/// Where `text` starts in the table (left of where the DEVICE panel starts at 120 columns, whose
/// NAME row would otherwise match a device name first).
fn table_text(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
    let wanted: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    (0..buf.area.height).find_map(|y| {
        let row = tui::cells(buf, y);
        let last = row.len().min(usize::from(DETAIL_X - 4)).checked_sub(wanted.len())?;
        (0..=last)
            .find(|&x| row[x..x + wanted.len()] == wanted[..])
            .map(|x| (x as u16, y))
    })
}

/// The table row showing `text`.
fn table_row(buf: &Buffer, text: &str) -> u16 {
    table_text(buf, text)
        .unwrap_or_else(|| panic!("no table row with {text:?}:\n{}", tui::rows(buf).join("\n")))
        .1
}

fn x_of(buf: &Buffer, text: &str) -> u16 {
    tui::find_text(buf, text)
        .unwrap_or_else(|| panic!("{text:?} not drawn:\n{}", tui::rows(buf).join("\n")))
        .0
}

/// The text of row `y` from column `from` to column `to` (exclusive), trimmed.
fn span(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    (from..to.min(buf.area.width))
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Whether the cell at `(x, y)` is in the cursor bar.
fn is_cursor(buf: &Buffer, x: u16, y: u16) -> bool {
    let style = tui::cell_style(buf, x, y);
    style.bg == Some(P.ink) && style.fg == Some(P.ground)
}

fn bold(buf: &Buffer, x: u16, y: u16) -> bool {
    tui::cell_style(buf, x, y).add_modifier.contains(Modifier::BOLD)
}

#[test]
fn devices_table_lists_local_audio_and_renderers_in_columns() {
    common::isolate();
    let mut app = devices_app("table", mockup_renderers());
    // The cursor off the rows the test reads styles from.
    app.devices_cursor = 1;
    let buf = tui::render(&mut app, 120, 40);

    let header = row_of(&buf, "ADDRESS");
    assert_eq!(span(&buf, header, NAME_X, TYPE_X), "NAME");
    assert_eq!(span(&buf, header, TYPE_X, ADDRESS_X), "TYPE");
    assert_eq!(span(&buf, header, ADDRESS_X, DETAIL_X - 4), "ADDRESS");
    assert_eq!(
        tui::cell_style(&buf, NAME_X, header).fg,
        Some(P.dim),
        "column names are DIM"
    );

    let local = table_row(&buf, "LOCAL AUDIO");
    assert_eq!(span(&buf, local, MARK_X, NAME_X), "●");
    assert_eq!(span(&buf, local, NAME_X, TYPE_X), "LOCAL AUDIO");
    assert_eq!(span(&buf, local, TYPE_X, ADDRESS_X), "MPV");
    assert_eq!(
        span(&buf, local, ADDRESS_X, DETAIL_X - 4),
        format!("{:<20}ACTIVE", "THIS MACHINE")
    );
    let active_x = x_of(&buf, "ACTIVE");
    assert!(bold(&buf, MARK_X, local) && bold(&buf, NAME_X, local) && bold(&buf, active_x, local));
    assert_eq!(tui::cell_style(&buf, TYPE_X, local).fg, Some(P.mid));

    let living = table_row(&buf, "LIVING ROOM");
    assert_eq!(span(&buf, living, MARK_X, NAME_X), "○");
    assert_eq!(tui::cell_style(&buf, MARK_X, living).fg, Some(P.mid));
    assert_eq!(span(&buf, living, TYPE_X, ADDRESS_X), "DLNA");
    assert_eq!(span(&buf, living, ADDRESS_X, DETAIL_X - 4), "192.168.1.60:1400");
    assert_eq!(tui::cell_style(&buf, ADDRESS_X, living).fg, Some(P.mid));
    let denon = table_row(&buf, "DENON AVR-X2800H");
    assert_eq!(span(&buf, denon, ADDRESS_X, DETAIL_X - 4), "192.168.1.72:8080");

    // Rows are double-spaced while they all fit that way (2d), in discovery order.
    let primare = table_row(&buf, "PRIMARE I15 PRISMA");
    assert_eq!(local, header + 2);
    assert_eq!(primare, local + 2);
    assert_eq!(living, primare + 2);
    assert_eq!(denon, living + 2);

    // Using a renderer moves the ● and ACTIVE to its row.
    let devices = app.device_manager.get_devices();
    app.device_manager.set_active_device(Some(devices[1].clone()));
    let buf = tui::render(&mut app, 120, 40);
    let living = table_row(&buf, "LIVING ROOM");
    let local = table_row(&buf, "LOCAL AUDIO");
    assert_eq!(span(&buf, living, MARK_X, NAME_X), "●");
    assert_eq!(
        span(&buf, living, ADDRESS_X, DETAIL_X - 4),
        "192.168.1.60:1400   ACTIVE"
    );
    assert_eq!(span(&buf, local, MARK_X, NAME_X), "○");
    assert_eq!(span(&buf, local, ADDRESS_X, DETAIL_X - 4), "THIS MACHINE");
}

#[test]
fn devices_rows_go_single_spaced_when_double_spacing_does_not_fit() {
    common::isolate();
    let many: Vec<DLNADevice> = (0..12)
        .map(|i| {
            renderer(
                &format!("Renderer {i:02}"),
                "10.0.0.9",
                1400 + i,
                "",
                "",
                &format!("uuid:r{i}"),
            )
        })
        .collect();
    let mut app = devices_app("single", many);
    let buf = tui::render(&mut app, 120, 30);
    let first = table_row(&buf, "RENDERER 00");
    assert_eq!(table_row(&buf, "RENDERER 01"), first + 1);
}

#[test]
fn devices_cursor_bar_follows_j_k_and_wraps() {
    common::isolate();
    let mut app = devices_app("cursor", mockup_renderers());
    let buf = tui::render(&mut app, 120, 40);
    let local = table_row(&buf, "LOCAL AUDIO");
    // The bar reaches one column past the text on each side (2d).
    assert!(is_cursor(&buf, MARK_X - 1, local) && is_cursor(&buf, NAME_X, local));
    assert!(is_cursor(&buf, DETAIL_X - 6, local));
    assert!(!is_cursor(&buf, DETAIL_X - 5, local));
    assert!(bold(&buf, NAME_X + 3, local));

    press(&mut app, KeyCode::Char('j'));
    let buf = tui::render(&mut app, 120, 40);
    assert!(!is_cursor(&buf, NAME_X, table_row(&buf, "LOCAL AUDIO")));
    assert!(is_cursor(&buf, NAME_X, table_row(&buf, "PRIMARE I15 PRISMA")));

    // k from the first row wraps to the last, j from the last back to the first.
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    let buf = tui::render(&mut app, 120, 40);
    assert!(is_cursor(&buf, NAME_X, table_row(&buf, "DENON AVR-X2800H")));
    press(&mut app, KeyCode::Down);
    let buf = tui::render(&mut app, 120, 40);
    assert!(is_cursor(&buf, NAME_X, table_row(&buf, "LOCAL AUDIO")));
}

#[test]
fn devices_footnote_counts_renderers_and_names_ssdp() {
    common::isolate();
    let mut app = devices_app("footnote", mockup_renderers());
    let buf = tui::render(&mut app, 120, 40);
    let foot = row_of(&buf, "3 FOUND  ·  SSDP 239.255.255.250:1900");
    assert_eq!(x_of(&buf, "3 FOUND"), MARK_X);
    assert_eq!(tui::cell_style(&buf, MARK_X, foot).fg, Some(P.dim));
    assert_eq!(row_of(&buf, "OUTPUT DEVICES") + 2, foot);

    let mut app = devices_app("footnote_none", Vec::new());
    let buf = tui::render(&mut app, 120, 40);
    row_of(&buf, "0 FOUND  ·  SSDP 239.255.255.250:1900");
    // Discovery done and nothing found: said so under Local Audio.
    let local = table_row(&buf, "LOCAL AUDIO");
    assert_eq!(row_of(&buf, "NO RENDERERS FOUND"), local + 2);
}

#[test]
fn scanning_note_shows_while_discovery_runs_and_not_after() {
    common::isolate();
    let mut app = devices_app("scanning", mockup_renderers());
    set_scanning(&app, true);
    let buf = tui::render(&mut app, 120, 40);
    let label = row_of(&buf, "OUTPUT DEVICES");
    let text = tui::row(&buf, label);
    let note = text.find("SCANNING ").expect("SCANNING note on the label row");
    let spinner = text[note + "SCANNING ".len()..].chars().next().unwrap();
    assert!(
        "⣾⣽⣻⢿⡿⣟⣯⣷".contains(spinner),
        "spinner frame after SCANNING, got {spinner:?}"
    );
    let (note_x, _) = tui::find_text(&buf, "SCANNING").unwrap();
    assert_eq!(tui::cell_style(&buf, note_x, label).fg, Some(P.dim));
    // The note ends where the panel's text does (2 columns short of the ` ▪`).
    assert_eq!(note_x + "SCANNING ⣾".chars().count() as u16, DETAIL_X - 4 - 2);
    let searching = row_of(&buf, "SEARCHING FOR DLNA / UPNP MEDIARENDERERS");
    assert_eq!(searching, table_row(&buf, "DENON AVR-X2800H") + 2);
    assert!(!tui::rows(&buf).iter().any(|r| r.contains("NO RENDERERS FOUND")));

    set_scanning(&app, false);
    let buf = tui::render(&mut app, 120, 40);
    let rows = tui::rows(&buf);
    assert!(!rows.iter().any(|r| r.contains("SCANNING")));
    assert!(!rows.iter().any(|r| r.contains("SEARCHING FOR")));
}

#[test]
fn spinner_turns_with_the_clock() {
    common::isolate();
    let frames: Vec<char> = (0..9).map(|i| kit::spinner(Duration::from_millis(100 * i))).collect();
    assert_eq!(frames[..8].iter().collect::<String>(), "⣾⣽⣻⢿⡿⣟⣯⣷");
    assert_eq!(frames[8], '⣾', "the frames repeat");
    assert_eq!(kit::spinner(Duration::from_millis(99)), '⣾');
}

#[test]
fn detail_panel_at_120_shows_the_selected_renderer() {
    common::isolate();
    let mut app = devices_app("detail", mockup_renderers());
    app.devices_cursor = 1;
    let buf = tui::render(&mut app, 120, 40);

    let label = row_of(&buf, "OUTPUT DEVICES");
    assert_eq!(span(&buf, label, DETAIL_X, 120), "DEVICE");
    assert!(bold(&buf, DETAIL_X, label));
    assert_eq!(span(&buf, label + 2, DETAIL_X, 120), "DLNA MEDIARENDERER");
    let header = row_of(&buf, "ADDRESS");
    let field = |buf: &Buffer, key: &str| -> String {
        let y = (header..40)
            .find(|&y| span(buf, y, DETAIL_X, DETAIL_VALUE_X) == key)
            .unwrap_or_else(|| panic!("no {key} row in the detail panel:\n{}", tui::rows(buf).join("\n")));
        assert_eq!(tui::cell_style(buf, DETAIL_X, y).fg, Some(P.dim), "{key} is DIM");
        span(buf, y, DETAIL_VALUE_X, 120)
    };
    // The detail rows start beside the table's column names (2d).
    assert_eq!(span(&buf, header, DETAIL_X, DETAIL_VALUE_X), "NAME");
    assert_eq!(field(&buf, "NAME"), "PRIMARE I15 PRISMA");
    assert_eq!(field(&buf, "MAKER"), "PRIMARE");
    assert_eq!(field(&buf, "MODEL"), "I15");
    assert_eq!(field(&buf, "HOST"), "192.168.1.145");
    assert_eq!(field(&buf, "PORT"), "8080");
    // Ids and paths keep their case; a long UDN is cut in the middle.
    let udn = field(&buf, "UDN");
    assert!(
        udn.starts_with("uuid:5f9e") && udn.ends_with("c41a") && udn.contains('…'),
        "{udn}"
    );
    assert_eq!(field(&buf, "CONTROL"), "/AVTransport/control");
    assert_eq!(field(&buf, "STATUS"), "NOT IN USE");
    let hint = row_of(&buf, "SWITCH OUTPUT HERE");
    assert_eq!(span(&buf, hint, DETAIL_X, 120), "ENTER  SWITCH OUTPUT HERE");
    let keycap = tui::cell_style(&buf, DETAIL_X, hint);
    assert_eq!((keycap.fg, keycap.bg), (theme::keycap().fg, theme::keycap().bg));

    // A renderer without a maker or model shows neither row.
    app.devices_cursor = 3;
    let buf = tui::render(&mut app, 120, 40);
    assert_eq!(field(&buf, "NAME"), "DENON AVR-X2800H");
    let detail: Vec<String> = (0..40).map(|y| span(&buf, y, DETAIL_X, DETAIL_VALUE_X)).collect();
    assert!(!detail.iter().any(|k| k == "MAKER" || k == "MODEL"), "{detail:?}");
}

#[test]
fn detail_panel_shows_volume_only_for_the_active_output() {
    common::isolate();
    let mut app = devices_app("volume", mockup_renderers());
    let detail_rows = |buf: &Buffer| -> Vec<String> { (0..40).map(|y| span(buf, y, DETAIL_X, 120)).collect() };

    // Local Audio is active: its volume is the player's.
    let buf = tui::render(&mut app, 120, 40);
    let rows = detail_rows(&buf);
    assert!(rows.iter().any(|r| r == "NAME        LOCAL AUDIO"), "{rows:?}");
    assert!(rows.iter().any(|r| r == "LOCAL PLAYBACK"), "{rows:?}");
    assert!(rows.iter().any(|r| r == "ENGINE      MPV"), "{rows:?}");
    assert!(rows.iter().any(|r| r == "STATUS      ACTIVE"), "{rows:?}");
    assert!(rows.iter().any(|r| r == "VOLUME      65"), "{rows:?}");
    assert!(
        !rows.iter().any(|r| r.contains("SWITCH OUTPUT HERE")),
        "already the output"
    );

    // An inactive renderer: no volume (the app doesn't know it).
    for cursor in 1..=3 {
        app.devices_cursor = cursor;
        let buf = tui::render(&mut app, 120, 40);
        let rows = tui::rows(&buf);
        assert!(
            !rows.iter().any(|r| r.contains("VOLUME")),
            "cursor {cursor}:\n{}",
            rows.join("\n")
        );
    }

    // The active renderer shows the volume the player reports, and Local Audio then has none.
    let devices = app.device_manager.get_devices();
    app.device_manager.set_active_device(Some(devices[0].clone()));
    app.devices_cursor = 1;
    let rows = detail_rows(&tui::render(&mut app, 120, 40));
    assert!(rows.iter().any(|r| r == "STATUS      ACTIVE"), "{rows:?}");
    assert!(rows.iter().any(|r| r == "VOLUME      65"), "{rows:?}");
    app.devices_cursor = 0;
    let rows = tui::rows(&tui::render(&mut app, 120, 40));
    assert!(!rows.iter().any(|r| r.contains("VOLUME")), "{}", rows.join("\n"));
}

#[test]
fn under_110_columns_the_detail_panel_hides_and_maker_joins_the_footnote() {
    common::isolate();
    let mut app = devices_app("medium", mockup_renderers());
    app.devices_cursor = 1;
    let buf = tui::render(&mut app, 100, 30);
    let rows = tui::rows(&buf);
    assert!(!rows
        .iter()
        .any(|r| r.contains("SWITCH OUTPUT HERE") || r.contains("MEDIARENDERER")));
    assert!(!rows.iter().any(|r| r.contains("CONTROL")));
    let label = tui::row(&buf, row_of(&buf, "OUTPUT DEVICES"));
    assert!(!label.contains("DEVICE "), "no DEVICE panel label: {label:?}");
    row_of(&buf, "3 FOUND  ·  SSDP 239.255.255.250:1900  ·  PRIMARE I15");

    // The table takes the whole width: the address column moves right with the name column.
    let primare = table_row(&buf, "PRIMARE I15 PRISMA");
    assert!(x_of(&buf, "192.168.1.145:8080") > ADDRESS_X);
    assert!(is_cursor(&buf, NAME_X, primare));
    assert!(is_cursor(&buf, 96, primare), "cursor bar spans the wider table");

    // Local Audio has no maker: the footnote is the count and SSDP only.
    app.devices_cursor = 0;
    let buf = tui::render(&mut app, 100, 30);
    let foot = tui::row(&buf, row_of(&buf, "3 FOUND"));
    assert_eq!(foot.trim(), "3 FOUND  ·  SSDP 239.255.255.250:1900");
}

#[test]
fn devices_render_at_120x40_100x30_and_80x24() {
    common::isolate();
    let mut app = devices_app("sizes", mockup_renderers());
    app.devices_cursor = 2;
    for (w, h) in [(120, 40), (100, 30), (80, 24)] {
        let buf = tui::render(&mut app, w, h);
        for name in ["LOCAL AUDIO", "PRIMARE I15 PRISMA", "LIVING ROOM", "DENON AVR-X2800H"] {
            table_row(&buf, name);
        }
        assert!(is_cursor(&buf, NAME_X, table_row(&buf, "LIVING ROOM")), "{w}x{h}");
        assert!(tui::find_text(&buf, "ACTIVE").is_some(), "{w}x{h}");
        assert!(tui::find_text(&buf, "USE OUTPUT").is_some(), "{w}x{h}: key row");
    }

    // 80×24: the footnote moves to the label row (3g); the table keeps its columns.
    let buf = tui::render(&mut app, 80, 24);
    let label = row_of(&buf, "OUTPUT DEVICES");
    assert!(
        tui::row(&buf, label).trim_end().ends_with("3 FOUND"),
        "{}",
        tui::row(&buf, label)
    );
    assert!(tui::find_row(&buf, "SSDP").is_none());
    let living = tui::row(&buf, table_row(&buf, "LIVING ROOM"));
    assert!(
        living.contains(" DLNA ") && living.contains("192.168.1.60:1400"),
        "{living}"
    );

    // Scanning at 80×24: the note joins the count.
    set_scanning(&app, true);
    let buf = tui::render(&mut app, 80, 24);
    let label = tui::row(&buf, row_of(&buf, "OUTPUT DEVICES"));
    assert!(label.contains("3 FOUND  ·  SCANNING "), "{label}");
}

#[test]
fn devices_key_row_matches_the_handlers() {
    common::isolate();
    let mut app = devices_app("keys", mockup_renderers());
    let hints: Vec<(String, String)> = app.tab_key_hints().into_iter().map(|h| (h.keys, h.label)).collect();
    let want: Vec<(String, String)> = [
        ("J K", "SELECT"),
        ("ENTER", "USE OUTPUT"),
        ("R", "RESCAN"),
        ("?", "KEYS"),
    ]
    .iter()
    .map(|(k, l)| (k.to_string(), l.to_string()))
    .collect();
    assert_eq!(hints, want);
    let buf = tui::render(&mut app, 120, 40);
    row_of(&buf, " J K  SELECT    ");
}

#[test]
fn narrow_devices_tab_does_not_panic_and_keeps_the_cursor_row_visible() {
    common::isolate();
    let many: Vec<DLNADevice> = (0..30)
        .map(|i| {
            renderer(
                &format!("R{i:02}"),
                "10.0.0.9",
                1400 + i,
                "Maker",
                "Model",
                &format!("uuid:r{i}"),
            )
        })
        .collect();
    let mut app = devices_app("narrow", many);
    set_scanning(&app, true);
    for (w, h) in [
        (120, 40),
        (100, 30),
        (85, 24),
        (79, 24),
        (60, 20),
        (40, 12),
        (30, 10),
        (20, 8),
        (10, 5),
    ] {
        for cursor in [0, 15, 30] {
            app.devices_cursor = cursor;
            let buf = tui::render(&mut app, w, h);
            if w < 30 || h < 10 {
                continue; // only that it draws without panicking
            }
            let name = if cursor == 0 {
                "LOCAL AUDIO".to_string()
            } else {
                format!("R{:02}", cursor - 1)
            };
            let (x, y) = table_text(&buf, &name).unwrap_or_else(|| {
                panic!(
                    "{w}x{h} cursor {cursor}: {name} not shown:\n{}",
                    tui::rows(&buf).join("\n")
                )
            });
            assert!(
                is_cursor(&buf, x, y),
                "{w}x{h} cursor {cursor}: {name} not under the cursor bar"
            );
        }
    }
}
