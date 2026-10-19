//! Help modal (`?`, mockup 2g): every key the handlers act on, in two columns of sections.
//!
//! Full layout (96 x 32, from 32 rows and 88 columns): a title row with the version, each section
//! under a label and a rule, and a `? ESC CLOSE` footer. Compact layout (at most 22 rows, so it
//! fits 80x24): the title in the top border, sections with no rules or blank rows between them.
//! Both stay inside the screen's 2-column gutters, except at 80-83 columns, where the compact
//! layout takes 80 columns so every description stays whole; under 80 the longest are cut with
//! `…`.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use super::kit::{self, KeyHint, ModalSpec};
use super::theme;

/// One section of the help: its label and its keys (keycap text, description).
struct Section {
    title: &'static str,
    keys: &'static [(&'static str, &'static str)],
}

/// The seek keys' description: drawn as `Seek -<step>s / +<step>s` with the settings' seek step
/// ([`seek_desc`]).
const SEEK_DESC: &str = "Seek -<step>s / +<step>s";

/// `Seek -30s / +30s` for a 30 s seek step.
fn seek_desc(step: f64) -> String {
    format!("Seek -{step}s / +{step}s")
}

/// Shift notation (`⇧R`) for capital letters; descriptions at most [`DESC_MAX`] characters.
const LEFT: [Section; 3] = [
    Section {
        title: "PLAYBACK & AUDIO",
        keys: &[
            ("SPC", "Play / Pause"),
            ("S", "Stop playback"),
            ("← →", "Previous / Next track"),
            ("[ ]", SEEK_DESC),
            ("+ −", "Volume Up / Down"),
        ],
    },
    Section {
        title: "NAVIGATION & TABS",
        keys: &[
            ("N", "Toggle Now Playing ↔ Queue"),
            ("1–5", "Tab: Now/Queue/Dev/Set/PL"),
            ("H L", "Focus panes (tabs 2 & 5)"),
            ("J K", "Move Up / Down (wraps)"),
            ("⇧J ⇧K", "Page (2) / Move track (5)"),
            ("G ⇧G", "Jump to Start / End"),
            ("PgUp/PgDn", "Page Up / Down (tabs 2 & 5)"),
            ("BKSP", "Go up to parent dir"),
            ("~", "Jump to startup home dir"),
        ],
    },
    Section {
        title: "PLAYLISTS (TAB 5)",
        keys: &[
            ("C ⇧R ⇧D", "New / Rename / Delete"),
            ("↵ A", "Open/Play / Queue playlist"),
        ],
    },
];

const RIGHT: [Section; 2] = [
    Section {
        title: "LIBRARY & QUEUE",
        keys: &[
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
        ],
    },
    Section {
        title: "SEARCH & DISCOVERY",
        keys: &[("/", "Spotlight jump in folder"), ("F", "Full library fuzzy search")],
    },
];

/// Columns of the key field: the keycap and at least one blank column after it.
const KEY_FIELD: u16 = 10;
/// Longest description, in columns.
const DESC_MAX: u16 = 27;
/// Width of the full layout.
const FULL_WIDTH: u16 = 96;
/// Narrowest box for the full layout: border, 3 columns of padding each side, and two columns
/// that each hold a key field, the longest description and a blank column.
const FULL_MIN_WIDTH: u16 = 2 + 2 * 3 + 2 * (KEY_FIELD + DESC_MAX + 1);
/// Narrowest box whose compact layout shows every description whole: as above, with the compact
/// frame's padding. 80, so the help fits an 80-column terminal whole.
const WHOLE_MIN_WIDTH: u16 = 2 + 2 * kit::COMPACT_MODAL_PAD_X + 2 * (KEY_FIELD + DESC_MAX + 1);

const _: () = assert!(WHOLE_MIN_WIDTH == 80, "the compact help must fit 80 columns whole");

/// Rows a column of `sections` takes: with a rule under each label and a blank row between
/// sections (full), or neither (compact).
const fn column_rows(sections: &[Section], full: bool) -> u16 {
    let mut rows = 0;
    let mut i = 0;
    while i < sections.len() {
        rows += 1 + sections[i].keys.len() as u16;
        if full {
            rows += 1; // the rule
            if i > 0 {
                rows += 1; // the blank row before the section
            }
        }
        i += 1;
    }
    rows
}

const fn max(a: u16, b: u16) -> u16 {
    if a > b {
        a
    } else {
        b
    }
}

/// Full height: border and padding rows (4), the title row and the blank row under it, the key
/// list, a blank row and the footer.
const FULL_HEIGHT: u16 = 4 + 2 + max(column_rows(&LEFT, true), column_rows(&RIGHT, true)) + 2;

/// Compact height: the border (2), the key list and the footer.
const COMPACT_HEIGHT: u16 = 2 + max(column_rows(&LEFT, false), column_rows(&RIGHT, false)) + 1;

const _: () = assert!(FULL_HEIGHT <= 32, "the full help must fit 32 rows");
const _: () = assert!(COMPACT_HEIGHT <= 22, "the compact help must fit an 80x24 terminal");

/// Draw the help modal centred on `screen`, inside its gutters ([`kit::modal_area`]), except on
/// 80-83 column screens: there the gutters are what keeps every description whole, so the help
/// takes [`WHOLE_MIN_WIDTH`] columns. `seek_step` is the settings' seek step in seconds.
pub(super) fn render(f: &mut Frame, screen: Rect, seek_step: f64) {
    let room = screen.width.saturating_sub(2 * kit::MODAL_MARGIN);
    let full = screen.height >= FULL_HEIGHT && room >= FULL_MIN_WIDTH;
    let height = if full { FULL_HEIGHT } else { COMPACT_HEIGHT };
    let area = if room < WHOLE_MIN_WIDTH && screen.width >= WHOLE_MIN_WIDTH {
        kit::centered(screen, WHOLE_MIN_WIDTH, height)
    } else {
        kit::modal_area(screen, FULL_WIDTH, height)
    };
    let inner = kit::modal_frame(
        f,
        area,
        ModalSpec {
            title: "KEYS",
            subtitle: None,
            note: Some(Line::from(format!("QUICKSTREAM {}", env!("CARGO_PKG_VERSION")))),
            error: false,
            compact: !full,
        },
    );
    if inner.is_empty() {
        return;
    }

    // The footer takes the last row (after a blank one in the full layout).
    let footer_rows = if full { 2 } else { 1 };
    let list_height = inner.height.saturating_sub(footer_rows);
    let left_width = inner.width.div_ceil(2);
    let left = Rect::new(inner.x, inner.y, left_width, list_height);
    let right = Rect::new(inner.x + left_width, inner.y, inner.width - left_width, list_height);
    draw_column(f, left, &LEFT, full, seek_step);
    draw_column(f, right, &RIGHT, full, seek_step);
    draw_footer(f, Rect::new(inner.x, inner.bottom() - 1, inner.width, 1));
}

/// `sections` from the top of `area`, as many rows as fit. A section's label is drawn only with
/// room for its first key under it, so a column cut short ends on a key, never on a label.
fn draw_column(f: &mut Frame, area: Rect, sections: &[Section], full: bool, seek_step: f64) {
    let buf = f.buffer_mut();
    let width = area.width as usize;
    let mut y = area.y;
    for (i, section) in sections.iter().enumerate() {
        if full && i > 0 {
            y += 1;
        }
        // The label, its rule (full layout) and the first key.
        let header_rows = if full { 2 } else { 1 };
        if y + header_rows >= area.bottom() {
            return;
        }
        buf.set_stringn(area.x, y, kit::clip(section.title, width), width, theme::label(true));
        y += 1;
        if full {
            // `────── ▪`, ending 4 columns short of the next column.
            kit::modal_rule(buf, area.x, y, area.width.saturating_sub(4));
            y += 1;
        }
        for (key, desc) in section.keys {
            if y >= area.bottom() {
                return;
            }
            if *desc == SEEK_DESC {
                draw_key(buf, area.x, y, width, key, &seek_desc(seek_step));
            } else {
                draw_key(buf, area.x, y, width, key, desc);
            }
            y += 1;
        }
    }
}

/// One key: the keycap (` KEY `, or the key alone when padded it would not leave a blank column)
/// in the 10-column key field, then the description in MID, cut to leave a blank column.
fn draw_key(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, width: usize, key: &str, desc: &str) {
    let field = KEY_FIELD as usize;
    let cap_room = field - 1;
    let cap = if kit::text_width(key) + 2 <= cap_room {
        format!(" {key} ")
    } else {
        kit::clip(key, cap_room)
    };
    buf.set_stringn(x, y, &cap, cap_room.min(width), theme::keycap());
    if width > field {
        let desc_room = width - field - 1;
        buf.set_stringn(x + KEY_FIELD, y, kit::clip(desc, desc_room), desc_room, theme::mid());
    }
}

/// The help's keys: ` ? ESC ` CLOSE (either key closes it). The footer and the screen's key row
/// both show these.
pub(super) fn footer_hints() -> Vec<KeyHint> {
    vec![KeyHint::new("? ESC", "CLOSE", 1)]
}

/// [`footer_hints`] against the right of `area`.
fn draw_footer(f: &mut Frame, area: Rect) {
    let line = kit::key_row(&footer_hints(), area.width, None);
    let width = line.width() as u16;
    let x = area.right().saturating_sub(width).max(area.x);
    f.buffer_mut().set_line(x, area.y, &line, area.right() - x);
}
