//! Modals over the Telemetry screen: the shared frame (`kit::dim_buffer`, `kit::modal_frame`,
//! `kit::centered`) and the help modal (mockup 2g, with a compact layout for 80x24).

mod common;

use std::fs;
use std::path::PathBuf;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;

use common::tui;
use quickstream::settings::SettingsManager;
use quickstream::ui::kit::{self, ModalSpec};
use quickstream::ui::playlists::{NamePurpose, PendingAdd, PlaylistModal};
use quickstream::ui::theme;
use quickstream::ui::AppUI;

fn modal_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_modals_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    (AppUI::new(Some(dir.clone()), settings_mgr), dir)
}

/// Draw `draw` into a `width` x `height` buffer that starts filled with `fill`.
fn draw_with(width: u16, height: u16, fill: Style, draw: impl FnOnce(&mut ratatui::Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            f.buffer_mut().set_style(area, fill);
            draw(f);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// The modal's outer rectangle: from the first `┌` to the `┘` closing it.
fn modal_rect(buffer: &Buffer) -> Rect {
    let (x, y) = tui::find_text(buffer, "┌").unwrap_or_else(|| panic!("no modal\n{}", tui::rows(buffer).join("\n")));
    let right = (x + 1..buffer.area.width)
        .find(|&cx| buffer[(cx, y)].symbol() == "┐")
        .expect("no ┐");
    let bottom = (y + 1..buffer.area.height)
        .find(|&cy| buffer[(x, cy)].symbol() == "└")
        .expect("no └");
    assert_eq!(buffer[(right, bottom)].symbol(), "┘", "modal border not closed");
    Rect::new(x, y, right - x + 1, bottom - y + 1)
}

/// The text of the rows of `rect`, each cut to its columns.
fn rect_rows(buffer: &Buffer, rect: Rect) -> Vec<String> {
    (rect.top()..rect.bottom())
        .map(|y| (rect.left()..rect.right()).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

fn assert_dimmed(buffer: &Buffer, x: u16, y: u16, what: &str) {
    let p = theme::palette();
    let cell = &buffer[(x, y)];
    assert_eq!(cell.fg, p.rule, "{what}: cell ({x}, {y}) fg");
    assert_eq!(cell.bg, p.ground, "{what}: cell ({x}, {y}) bg");
    assert!(
        cell.modifier.is_empty(),
        "{what}: cell ({x}, {y}) has {:?}",
        cell.modifier
    );
}

// ---------------------------------------------------------------------------------------------
// kit: dim_buffer, centered, modal_frame
// ---------------------------------------------------------------------------------------------

#[test]
fn dim_buffer_sets_rule_on_ground_and_clears_modifiers() {
    common::isolate();
    let p = theme::palette();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
    buffer.set_string(0, 0, "QUICKSTREAM", theme::cursor());
    buffer.set_string(
        0,
        1,
        "MIRA LENZ",
        theme::ink().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    );
    buffer.set_string(0, 3, "outside", theme::keycap());

    kit::dim_buffer(&mut buffer, Rect::new(0, 0, 10, 3));
    for y in 0..3 {
        for x in 0..10 {
            assert_dimmed(&buffer, x, y, "dimmed area");
        }
    }
    // The text stays; only its style changes.
    assert!(tui::row(&buffer, 0).starts_with("QUICKSTREA"));
    // Cells outside the area are left alone.
    assert_eq!(buffer[(0, 3)].bg, p.rule);
    assert_eq!(buffer[(0, 3)].fg, p.ink);

    // An area reaching past the buffer is clipped, not a panic.
    kit::dim_buffer(&mut buffer, Rect::new(5, 2, 50, 50));
    assert_dimmed(&buffer, 9, 3, "clipped area");
}

#[test]
fn centered_rect_is_centred_and_never_larger_than_the_area() {
    common::isolate();
    let screen = Rect::new(0, 0, 120, 40);
    assert_eq!(kit::centered(screen, 96, 32), Rect::new(12, 4, 96, 32));
    // Larger than the area: clamped to it.
    assert_eq!(kit::centered(Rect::new(0, 0, 80, 24), 96, 32), Rect::new(0, 0, 80, 24));
    // An offset area keeps its offset.
    assert_eq!(kit::centered(Rect::new(10, 5, 20, 10), 10, 4), Rect::new(15, 8, 10, 4));
    // Tiny and empty areas: no panic, never outside.
    for area in [Rect::new(0, 0, 0, 0), Rect::new(0, 0, 1, 1), Rect::new(3, 2, 2, 1)] {
        let r = kit::centered(area, 96, 32);
        assert!(area.union(r) == area || r.is_empty(), "{r:?} outside {area:?}");
    }
}

#[test]
fn modal_frame_draws_a_dim_border_and_a_reverse_video_title_chip() {
    common::isolate();
    let p = theme::palette();
    let mut inner = Rect::default();
    let buffer = draw_with(60, 16, theme::base(), |f| {
        f.buffer_mut().set_string(0, 5, "x".repeat(60), theme::ink());
        inner = kit::modal_frame(
            f,
            Rect::new(5, 2, 50, 12),
            ModalSpec {
                title: "KEYS",
                subtitle: None,
                note: Some(Line::from("QUICKSTREAM 1.0")),
                error: false,
                compact: false,
            },
        );
    });

    // Thin border in DIM.
    for (x, y, glyph) in [
        (5, 2, "┌"),
        (54, 2, "┐"),
        (5, 13, "└"),
        (54, 13, "┘"),
        (20, 2, "─"),
        (5, 8, "│"),
    ] {
        assert_eq!(buffer[(x, y)].symbol(), glyph, "({x}, {y})");
        assert_eq!(buffer[(x, y)].fg, p.dim, "({x}, {y}) border colour");
    }
    // Title chip: 3 columns of padding, a row below the border, reverse video.
    let (x, y) = tui::find_text(&buffer, " KEYS ").expect("title chip");
    assert_eq!((x, y), (5 + 1 + 3, 2 + 2));
    for style in tui::text_styles(&buffer, " KEYS ").unwrap() {
        assert_eq!(style.fg, Some(p.ground));
        assert_eq!(style.bg, Some(p.ink));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }
    // The note against the right edge, inside the padding, in DIM.
    let (nx, ny) = tui::find_text(&buffer, "QUICKSTREAM 1.0").expect("note");
    assert_eq!(ny, y);
    assert_eq!(nx + 15, 54 - 3, "note ends 3 columns before the border");
    assert!(tui::text_styles(&buffer, "QUICKSTREAM 1.0")
        .unwrap()
        .iter()
        .all(|s| s.fg == Some(p.dim)));

    // The content area: inside the padding, a blank row under the title.
    assert_eq!(inner, Rect::new(9, 6, 42, 6));
    // Cleared: nothing drawn before the modal shows through, and every cell is on GROUND.
    assert!(
        (6..54).all(|x| buffer[(x, 5)].symbol() != "x"),
        "{:?}",
        tui::row(&buffer, 5)
    );
    for y in 2..14 {
        for x in 5..55 {
            assert_eq!(
                buffer[(x, y)].bg,
                if (9..15).contains(&x) && y == 4 {
                    p.ink
                } else {
                    p.ground
                }
            );
        }
    }
}

#[test]
fn modal_frame_error_border_and_chip_in_error_colour() {
    common::isolate();
    let p = theme::palette();
    let buffer = draw_with(40, 10, theme::base(), |f| {
        kit::modal_frame(
            f,
            Rect::new(0, 0, 40, 10),
            ModalSpec {
                title: "DELETE",
                subtitle: None,
                note: None,
                error: true,
                compact: false,
            },
        );
    });
    assert_eq!(buffer[(0, 0)].fg, p.error);
    assert_eq!(buffer[(39, 9)].fg, p.error);
    for style in tui::text_styles(&buffer, " DELETE ").unwrap() {
        assert_eq!(style.fg, Some(p.ground));
        assert_eq!(style.bg, Some(p.error));
    }
}

#[test]
fn compact_modal_frame_puts_the_title_in_the_border() {
    common::isolate();
    let mut inner = Rect::default();
    let buffer = draw_with(60, 12, theme::base(), |f| {
        inner = kit::modal_frame(
            f,
            Rect::new(0, 0, 60, 12),
            ModalSpec {
                title: "KEYS",
                subtitle: None,
                note: Some(Line::from("QUICKSTREAM 1.0")),
                error: false,
                compact: true,
            },
        );
    });
    let top = tui::row(&buffer, 0);
    assert!(top.starts_with("┌─ KEYS ─"), "{top:?}");
    assert!(top.ends_with(" QUICKSTREAM 1.0 ─┐"), "{top:?}");
    // One column of padding, no padding rows.
    assert_eq!(inner, Rect::new(2, 1, 56, 10));
}

#[test]
fn modal_frame_subtitle_follows_the_chip_and_is_cut_from_its_start() {
    common::isolate();
    let p = theme::palette();
    let spec = |compact| ModalSpec {
        title: "SEARCH",
        subtitle: Some("~/Music/Mira Lenz/Low Tide Atlas"),
        note: Some(Line::from(vec![Span::styled("SCANNING ⣾", theme::mid())])),
        error: false,
        compact,
    };
    // Room for everything: chip, 2 columns, the subtitle in MID, the note at the right.
    let buffer = draw_with(80, 8, theme::base(), |f| {
        kit::modal_frame(f, f.area(), spec(false));
    });
    let row = tui::row(&buffer, 2);
    assert!(row.contains(" SEARCH   ~/Music/Mira Lenz/Low Tide Atlas"), "{row:?}");
    assert!(row.contains("SCANNING ⣾   │"), "{row:?}");
    let (x, y) = tui::find_text(&buffer, "~/Music").unwrap();
    assert_eq!(buffer[(x, y)].fg, p.mid);
    assert_eq!(buffer[(x - 1, y)].bg, p.ground, "a gap after the chip");

    // Narrow: the start of the subtitle goes, the note stays.
    let buffer = draw_with(44, 8, theme::base(), |f| {
        kit::modal_frame(f, f.area(), spec(false));
    });
    let row = tui::row(&buffer, 2);
    assert!(row.contains("…") && row.contains("Atlas"), "{row:?}");
    assert!(!row.contains("~/Music"), "{row:?}");
    assert!(row.contains("SCANNING ⣾"), "{row:?}");

    // Compact: in the top border, spaced off the `─`.
    let buffer = draw_with(80, 8, theme::base(), |f| {
        kit::modal_frame(f, f.area(), spec(true));
    });
    let top = tui::row(&buffer, 0);
    assert!(
        top.starts_with("┌─ SEARCH ─ ~/Music/Mira Lenz/Low Tide Atlas ─"),
        "{top:?}"
    );
    assert!(top.ends_with(" SCANNING ⣾ ─┐"), "{top:?}");

    // No room for the note: the subtitle takes its place.
    let buffer = draw_with(26, 8, theme::base(), |f| {
        kit::modal_frame(f, f.area(), spec(false));
    });
    let row = tui::row(&buffer, 2);
    assert!(row.contains(" SEARCH   …") && row.contains("Atlas"), "{row:?}");
    assert!(!row.contains("SCANNING"), "{row:?}");

    // No room at all: left out.
    let buffer = draw_with(20, 8, theme::base(), |f| {
        kit::modal_frame(f, f.area(), spec(false));
    });
    let row = tui::row(&buffer, 2);
    assert!(!row.contains('~') && !row.contains("Atlas"), "{row:?}");
}

#[test]
fn modal_frame_never_panics_at_tiny_sizes() {
    common::isolate();
    for (w, h) in [(1, 1), (2, 2), (3, 3), (8, 4), (12, 6)] {
        for compact in [false, true] {
            let mut inner = Rect::default();
            draw_with(w, h, theme::base(), |f| {
                inner = kit::modal_frame(
                    f,
                    f.area(),
                    ModalSpec {
                        title: "SEARCH",
                        subtitle: None,
                        note: Some(Line::from(vec![Span::raw("SCANNING ⣾")])),
                        error: false,
                        compact,
                    },
                );
            });
            let area = Rect::new(0, 0, w, h);
            assert!(inner.is_empty() || area.union(inner) == area, "{inner:?} at {w}x{h}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The tab under a modal is dimmed
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modal {
    Help,
    MixPicker,
    Search,
    NamePrompt,
    ConfirmDelete,
    AddPicker,
}

const MODALS: [Modal; 6] = [
    Modal::Help,
    Modal::MixPicker,
    Modal::Search,
    Modal::NamePrompt,
    Modal::ConfirmDelete,
    Modal::AddPicker,
];

fn open_modal(app: &mut AppUI, modal: Modal) {
    match modal {
        Modal::Help => app.show_help_modal = true,
        Modal::MixPicker => app.show_mix_picker_modal = true,
        Modal::Search => app.open_fuzzy_search(),
        Modal::NamePrompt => {
            app.playlists.modal = Some(PlaylistModal::NamePrompt {
                purpose: NamePurpose::Create,
                input: "Road".to_string(),
                error: None,
            })
        }
        Modal::ConfirmDelete => {
            app.playlists.modal = Some(PlaylistModal::ConfirmDelete {
                name: "Road Trip".to_string(),
                count: 3,
            })
        }
        Modal::AddPicker => {
            app.playlists.modal = Some(PlaylistModal::AddPicker {
                pending: PendingAdd::Entries {
                    label: "Halcyon Drift".to_string(),
                    entries: Vec::new(),
                },
                cursor: 0,
            })
        }
    }
}

fn close_modals(app: &mut AppUI) {
    app.show_help_modal = false;
    app.show_mix_picker_modal = false;
    if let Some(mut search) = app.search_modal.take() {
        search.close();
    }
    app.playlists.modal = None;
}

/// The key row: the last row with text.
fn key_row(buffer: &Buffer) -> u16 {
    (0..buffer.area.height)
        .rev()
        .find(|&y| !tui::row(buffer, y).trim().is_empty())
        .unwrap_or_else(|| panic!("no key row\n{}", tui::rows(buffer).join("\n")))
}

/// Whether the key row `y` is drawn as usual: keycaps INK on RULE, labels MID, nothing in the
/// dimmed RULE-on-GROUND.
fn assert_key_row_undimmed(buffer: &Buffer, y: u16, what: &str) {
    let p = theme::palette();
    let row = tui::row(buffer, y);
    let mut keycaps = 0;
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, y)];
        if cell.symbol().trim().is_empty() {
            continue;
        }
        assert_ne!(cell.fg, p.rule, "{what}: ({x}, {y}) drawn dimmed: {row:?}");
        if cell.bg == p.rule {
            assert_eq!(cell.fg, p.ink, "{what}: keycap ({x}, {y}): {row:?}");
            keycaps += 1;
        }
    }
    assert!(keycaps > 0, "{what}: keycaps drawn: {row:?}");
}

#[test]
fn every_modal_dims_the_screen_under_it_but_not_the_key_row() {
    common::isolate();
    let (mut app, dir) = modal_app("dim_all");
    app.active_tab = 1;
    for modal in MODALS {
        open_modal(&mut app, modal);
        let buffer = tui::render(&mut app, 120, 40);
        // The header (top row naming QUICKSTREAM), the tab row (active chip included) and the key
        // row (the last row with text) sit outside every modal at 120x40.
        let header = tui::find_row(&buffer, "QUICKSTREAM").expect("header");
        let tabs = tui::find_row(&buffer, "1 NOW PLAYING").expect("tab row");
        let keys = key_row(&buffer);
        assert!(header < 4 && tabs < 4 && keys > 36, "{modal:?}: {header} {tabs} {keys}");
        for y in [header, tabs] {
            for x in 0..120 {
                assert_dimmed(&buffer, x, y, &format!("{modal:?} row {y}"));
            }
        }
        // The key row shows the modal's keys and the status, so it stays as it is drawn.
        assert_key_row_undimmed(&buffer, keys, &format!("{modal:?}"));
        close_modals(&mut app);
    }

    // Closed: the screen is drawn normally again.
    let buffer = tui::render(&mut app, 120, 40);
    let styles = tui::text_styles(&buffer, "QUICKSTREAM").unwrap();
    assert!(styles.iter().all(|s| s.fg == Some(theme::palette().ink)), "{styles:?}");
    assert!(styles.iter().all(|s| s.add_modifier.contains(Modifier::BOLD)));
    let _ = fs::remove_dir_all(&dir);
}

/// The status message stays readable while a modal is open: search's `Tab` adds a track and
/// says so only there, and errors keep their ERROR colour.
#[test]
fn the_status_message_shows_undimmed_under_a_modal() {
    common::isolate();
    let (mut app, dir) = modal_app("status_under_modal");
    app.active_tab = 1;
    let p = theme::palette();
    for modal in MODALS {
        for (status, colour) in [
            ("Added to queue: Halcyon Drift", p.mid),
            ("Couldn't read /Volumes/Music/Gone", p.error),
        ] {
            open_modal(&mut app, modal);
            app.status_message = status.to_string();
            let buffer = tui::render(&mut app, 120, 40);
            let keys = key_row(&buffer);
            let (x, y) = tui::find_text(&buffer, &status[..12])
                .unwrap_or_else(|| panic!("{modal:?}: {status:?} not shown\n{}", tui::rows(&buffer).join("\n")));
            assert_eq!(y, keys, "{modal:?}: on the key row");
            assert_eq!(buffer[(x, y)].fg, colour, "{modal:?}: {status:?}");
            close_modals(&mut app);
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_modal_keeps_the_gutters() {
    common::isolate();
    // However narrow the screen, a modal leaves the 2-column gutters on both sides alone.
    let (mut app, dir) = modal_app("gutters");
    app.active_tab = 1;
    for modal in MODALS {
        open_modal(&mut app, modal);
        for (w, h) in [
            (120, 40),
            (100, 30),
            (85, 30),
            (84, 24),
            (83, 24),
            (80, 24),
            (70, 24),
            (80, 20),
            (40, 12),
        ] {
            let buffer = tui::render(&mut app, w, h);
            let rect = modal_rect(&buffer);
            if modal == Modal::Help && (80..84).contains(&w) {
                // The one exception: at 80-83 columns the help takes 80, the gutters included,
                // because only then is every description whole (the 80x24 guarantee).
                assert_eq!((rect.x, rect.width), ((w - 80) / 2, 80), "{w}x{h}: {rect:?}");
                continue;
            }
            assert!(
                rect.x >= 2 && rect.right() <= w - 2,
                "{modal:?} at {w}x{h}: {rect:?}\n{}",
                tui::rows(&buffer).join("\n")
            );
        }
        close_modals(&mut app);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn modal_area_is_centred_inside_the_gutters() {
    common::isolate();
    // Wide enough: the asked size, centred.
    assert_eq!(
        kit::modal_area(Rect::new(0, 0, 120, 40), 96, 32),
        Rect::new(12, 4, 96, 32)
    );
    // Narrower than the modal plus the gutters: 2 columns left on each side.
    assert_eq!(
        kit::modal_area(Rect::new(0, 0, 85, 30), 96, 22),
        Rect::new(2, 4, 81, 22)
    );
    assert_eq!(
        kit::modal_area(Rect::new(0, 0, 80, 24), 84, 17),
        Rect::new(2, 3, 76, 17)
    );
    // Taller than the screen: clamped to it.
    assert_eq!(
        kit::modal_area(Rect::new(0, 0, 40, 12), 84, 17),
        Rect::new(2, 0, 36, 12)
    );
    // Tiny screens: no panic, never outside.
    for area in [Rect::new(0, 0, 0, 0), Rect::new(0, 0, 3, 1), Rect::new(3, 2, 4, 1)] {
        let r = kit::modal_area(area, 96, 32);
        assert!(area.union(r) == area || r.is_empty(), "{r:?} outside {area:?}");
    }
}

#[test]
fn help_dims_every_cell_outside_it() {
    common::isolate();
    let (mut app, dir) = modal_app("dim_help");
    app.show_help_modal = true;
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        let rect = modal_rect(&buffer);
        // At 80x24 the compact help covers the key row; at 120x40 the key row is under it.
        let keys = key_row(&buffer);
        let keys_shown = keys >= rect.bottom();
        assert_eq!(keys_shown, (w, h) == (120, 40), "{w}x{h}");
        if keys_shown {
            assert_key_row_undimmed(&buffer, keys, &format!("{w}x{h}"));
        }
        for y in (0..h).filter(|&y| !keys_shown || y != keys) {
            for x in 0..w {
                if !rect.contains((x, y).into()) {
                    assert_dimmed(&buffer, x, y, &format!("{w}x{h}"));
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Help modal (2g)
// ---------------------------------------------------------------------------------------------

/// Every key the help lists, as its keycap reads, with today's description (the seek step at
/// its default, 10 s).
const HELP_KEYS: [(&str, &str); 31] = [
    ("SPC", "Play / Pause"),
    ("S", "Stop playback"),
    ("← →", "Previous / Next track"),
    ("[ ]", "Seek -10s / +10s"),
    ("+ −", "Volume Up / Down"),
    ("N", "Toggle Now Playing ↔ Queue"),
    ("1–5", "Tab: Now/Queue/Dev/Set/PL"),
    ("H L", "Focus panes (tabs 2 & 5)"),
    ("J K", "Move Up / Down (wraps)"),
    ("⇧J ⇧K", "Page (2) / Move track (5)"),
    ("G ⇧G", "Jump to Start / End"),
    ("PgUp/PgDn", "Page Up / Down (tabs 2 & 5)"),
    ("BKSP", "Go up to parent dir"),
    ("~", "Jump to startup home dir"),
    ("C ⇧R ⇧D", "New / Rename / Delete"),
    ("↵ A", "Open/Play / Queue playlist"),
    ("ENTER", "Open / Queue / Play"),
    ("P", "Autopilot (adds if playing)"),
    ("⇧P", "Autopilot Reshuffle (next+)"),
    ("⇧M", "Mix Picker modal (vibe/BPM)"),
    ("R", "Seed Radio / Rescan dev"),
    ("Z", "Shuffle current queue"),
    ("A", "Add all songs in folder"),
    ("⇧L", "Add to playlist"),
    ("D X", "Remove track / bookmark"),
    ("C", "Clear queue (queue pane)"),
    ("B", "Bookmark folder in Homes"),
    ("⇧H", "Set folder as startup home"),
    ("V", "Toggle tags vs filenames"),
    ("/", "Spotlight jump in folder"),
    ("F", "Full library fuzzy search"),
];

const HELP_SECTIONS: [&str; 5] = [
    "PLAYBACK & AUDIO",
    "NAVIGATION & TABS",
    "PLAYLISTS (TAB 5)",
    "LIBRARY & QUEUE",
    "SEARCH & DISCOVERY",
];

/// Each key with its description on one row inside the modal: the keycap first, in the keycap
/// style, then the description in MID, 10 columns after the key field starts. `whole`: every
/// description in full; otherwise a description may be cut, ending in `…`.
fn assert_help_lists_every_key(buffer: &Buffer, size: &str, whole: bool) {
    let p = theme::palette();
    let rect = modal_rect(buffer);
    let rows = rect_rows(buffer, rect);
    for (key, desc) in HELP_KEYS {
        // The description, or (when cuts are allowed) its longest start that is drawn with `…`.
        let shown = if whole {
            vec![desc.to_string()]
        } else {
            let chars: Vec<char> = desc.chars().collect();
            let mut forms = vec![desc.to_string()];
            forms.extend(
                (8..chars.len())
                    .rev()
                    .map(|n| format!("{}…", chars[..n].iter().collect::<String>())),
            );
            forms
        };
        let (y, row, text) = shown
            .iter()
            .find_map(|text| {
                rows.iter()
                    .enumerate()
                    .find(|(_, row)| row.contains(text.as_str()))
                    .map(|(y, row)| (y, row, text))
            })
            .unwrap_or_else(|| panic!("{size}: {desc:?} missing\n{}", rows.join("\n")));
        let desc_col = row[..row.find(text.as_str()).unwrap()].chars().count();
        let field: String = row.chars().take(desc_col).collect::<Vec<_>>()[desc_col - 10..]
            .iter()
            .collect();
        assert_eq!(field.trim(), key, "{size}: key of {desc:?}: {row:?}");
        let y = rect.y + y as u16;
        let key_x = rect.x + (desc_col - 10) as u16;
        assert_eq!(buffer[(key_x, y)].bg, p.rule, "{size}: keycap of {desc:?}");
        let desc_x = rect.x + desc_col as u16;
        assert_eq!(buffer[(desc_x, y)].fg, p.mid, "{size}: {desc:?} in MID");
    }
    for section in HELP_SECTIONS {
        assert!(rows.iter().any(|r| r.contains(section)), "{size}: {section:?} missing");
        let styles = tui::text_styles(buffer, section).unwrap();
        assert!(
            styles.iter().all(|s| s.add_modifier.contains(Modifier::BOLD)),
            "{size}: {section:?}"
        );
    }
    if whole {
        // Nothing cut short.
        assert!(
            !rows.iter().any(|r| r.contains('…')),
            "{size}: clipped\n{}",
            rows.join("\n")
        );
    }
}

#[test]
fn help_lists_every_key_with_its_description_at_120x40_and_80x24() {
    common::isolate();
    let (mut app, dir) = modal_app("help_keys");
    app.show_help_modal = true;
    for (w, h) in [(120, 40), (80, 24), (100, 40), (96, 32), (88, 32), (84, 24), (83, 24)] {
        let buffer = tui::render(&mut app, w, h);
        assert_help_lists_every_key(&buffer, &format!("{w}x{h}"), true);
    }
    // Narrower, inside the gutters, every key is still listed, the longest descriptions cut
    // with `…`.
    let buffer = tui::render(&mut app, 70, 24);
    assert_help_lists_every_key(&buffer, "70x24", false);
    let _ = fs::remove_dir_all(&dir);
}

/// The seek keys' description names the seek step the settings use, not a fixed 10 s.
#[test]
fn help_names_the_seek_step_from_the_settings() {
    common::isolate();
    let (mut app, dir) = modal_app("help_seek_step");
    app.show_help_modal = true;
    for (step, text) in [
        (30.0, "Seek -30s / +30s"),
        (5.0, "Seek -5s / +5s"),
        (2.5, "Seek -2.5s / +2.5s"),
    ] {
        app.settings_mgr.settings.seek_step_seconds = step;
        for (w, h) in [(120, 40), (80, 24)] {
            let buffer = tui::render(&mut app, w, h);
            let rows = tui::rows(&buffer).join("\n");
            assert!(rows.contains(text), "{w}x{h}: {text:?} missing\n{rows}");
            assert!(!rows.contains("-10s"), "{w}x{h}: {rows}");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn help_frame_has_a_dim_border_and_a_keys_chip() {
    common::isolate();
    let (mut app, dir) = modal_app("help_frame");
    app.show_help_modal = true;
    let p = theme::palette();
    let version = format!("QUICKSTREAM {}", env!("CARGO_PKG_VERSION"));
    for (w, h) in [(120, 40), (80, 24)] {
        let buffer = tui::render(&mut app, w, h);
        let rect = modal_rect(&buffer);
        assert_eq!(buffer[(rect.x, rect.y)].fg, p.dim, "{w}x{h}: border in DIM");
        assert_eq!(buffer[(rect.right() - 1, rect.bottom() - 1)].fg, p.dim);
        let chip = tui::text_styles(&buffer, " KEYS ").unwrap_or_else(|| panic!("{w}x{h}: no chip"));
        assert!(
            chip.iter().all(|s| s.fg == Some(p.ground) && s.bg == Some(p.ink)),
            "{w}x{h}: chip {chip:?}"
        );
        let note = tui::text_styles(&buffer, &version).unwrap_or_else(|| panic!("{w}x{h}: no {version:?}"));
        assert!(note.iter().all(|s| s.fg == Some(p.dim)), "{w}x{h}");
        // Closing keys, as keycaps.
        let close = tui::text_styles(&buffer, " ESC ").unwrap();
        assert!(close.iter().all(|s| s.bg == Some(p.rule)), "{w}x{h}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn help_full_layout_at_120x40_matches_2g() {
    common::isolate();
    let (mut app, dir) = modal_app("help_full");
    app.show_help_modal = true;
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    // 96 wide, 32 tall, centred.
    assert_eq!(rect, Rect::new(12, 4, 96, 32));
    let rows = rect_rows(&buffer, rect);
    // Title row: a blank row under the border, the chip after 3 columns of padding.
    assert!(rows[1].trim_matches(['│', ' ']).is_empty(), "{:?}", rows[1]);
    assert!(rows[2].starts_with("│    KEYS "), "{:?}", rows[2]);
    // Each section has its rule ending in ` ▪`, in INK.
    let (sx, sy) = tui::find_text(&buffer, "PLAYBACK & AUDIO").unwrap();
    assert_eq!(sx, rect.x + 1 + 3);
    let rule = tui::row(&buffer, sy + 1);
    assert!(rule[rule.find('─').unwrap()..].starts_with("──────"), "{rule:?}");
    assert!(rule.contains("─ ▪"), "{rule:?}");
    assert_eq!(buffer[(sx, sy + 1)].fg, theme::palette().ink);
    // The second column starts halfway.
    let (lx, _) = tui::find_text(&buffer, "LIBRARY & QUEUE").unwrap();
    assert_eq!(lx, sx + 44);
    // Footer `? ESC CLOSE` (the key row's words) against the right, above the bottom padding
    // row.
    let footer = &rows[rows.len() - 3];
    assert!(
        footer.trim_end_matches(['│', ' ']).ends_with(" ? ESC  CLOSE"),
        "{footer:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn help_compact_layout_fits_80x24() {
    common::isolate();
    let (mut app, dir) = modal_app("help_compact");
    app.show_help_modal = true;
    let buffer = tui::render(&mut app, 80, 24);
    let rect = modal_rect(&buffer);
    assert!(rect.height <= 22, "{rect:?}");
    // 80 columns wide: the gutters are the room every description needs whole.
    assert_eq!((rect.x, rect.width), (0, 80), "{rect:?}");
    // From 84 columns it keeps them.
    let buffer = tui::render(&mut app, 84, 24);
    let rect = modal_rect(&buffer);
    assert_eq!((rect.x, rect.width), (2, 80), "{rect:?}");
    // The title sits in the top border.
    let rows = rect_rows(&buffer, rect);
    assert!(rows[0].starts_with("┌─ KEYS "), "{:?}", rows[0]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn help_never_panics_on_small_terminals() {
    common::isolate();
    let (mut app, dir) = modal_app("help_small");
    app.show_help_modal = true;
    for (w, h) in [(70, 24), (40, 12), (80, 31), (83, 40), (20, 6)] {
        let buffer = tui::render(&mut app, w, h);
        // The chip is still there, whatever else had to go.
        assert!(
            tui::find_text(&buffer, "KEYS").is_some(),
            "{w}x{h}\n{}",
            tui::rows(&buffer).join("\n")
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn help_colours_stay_off_basic_ansi() {
    common::isolate();
    // The restyled help uses palette colours only (no Yellow/Cyan from the old modal).
    let (mut app, dir) = modal_app("help_palette");
    app.show_help_modal = true;
    let p = theme::palette();
    let allowed = [p.ground, p.ink, p.mid, p.dim, p.rule];
    let buffer = tui::render(&mut app, 120, 40);
    let rect = modal_rect(&buffer);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let cell = &buffer[(x, y)];
            assert!(
                allowed.contains(&cell.fg) && allowed.contains(&cell.bg),
                "({x}, {y}) {:?} on {:?}",
                cell.fg,
                cell.bg
            );
            assert_ne!(cell.fg, Color::Reset);
        }
    }
    let _ = fs::remove_dir_all(&dir);
}
