//! The autopilot mix picker (`M`, mockup 2i): one row per profile the picker offers
//! ([`MixProfile::picker_profiles`]), its text taken from the profile itself (title, description
//! with the BPM range from the code), `●` on the profile in use and `○` on the others.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::Frame;

use super::kit::{self, Col, KeyHint, ModalSpec, RowMark};
use super::theme;
use crate::analysis::MixProfile;

/// Mockup 2i's width.
const WIDTH: u16 = 84;

/// Full: border and padding (6 rows with the title row and the blank under it), the subtitle,
/// rule and a blank row, each profile's two rows with a blank row between them, then a blank row
/// and the footer.
const FULL_HEIGHT: u16 = 6 + 3 + 5 * 3 - 1 + 2;

/// Compact (short terminals, 80x24): the title in the border, no padding or blank rows between
/// the profiles.
const COMPACT_HEIGHT: u16 = 2 + 3 + 5 * 2 + 2;

/// Columns of the index (`1`) and of the `●`/`○` before a profile's title; its description lines
/// up with the title.
const INDEX_COLS: u16 = 3;
const GLYPH_COLS: u16 = 3;

/// Draw the mix picker centred on the screen; `cursor` is the highlighted row and `active` the
/// profile in use.
pub(super) fn render(f: &mut Frame, cursor: usize, active: &MixProfile) {
    let screen = f.area();
    let full = screen.height >= FULL_HEIGHT;
    let area = kit::centered(screen, WIDTH, if full { FULL_HEIGHT } else { COMPACT_HEIGHT });
    let inner = kit::modal_frame(
        f,
        area,
        ModalSpec {
            title: "AUTOPILOT MIX",
            compact: !full,
            ..Default::default()
        },
    );
    if inner.is_empty() {
        return;
    }
    let width = inner.width as usize;
    let bottom = inner.bottom();
    let mut y = inner.y;

    let buf = f.buffer_mut();
    buf.set_stringn(
        inner.x,
        y,
        kit::clip("Sequences the queue with DJ spacing and transitions.", width),
        width,
        theme::mid(),
    );
    y += 1;
    if y < bottom {
        kit::modal_rule(buf, inner.x, y, inner.width);
    }
    y += 2;

    // The footer takes the last row, after a blank one.
    let rows_bottom = bottom.saturating_sub(2);
    let active_index = active.picker_index();
    for (index, profile) in MixProfile::picker_profiles().iter().enumerate() {
        if y >= rows_bottom {
            break;
        }
        let is_active = index == active_index;
        let (glyph, glyph_style, title_style) = if is_active {
            ("●", bold_ink(), bold_ink())
        } else {
            ("○", theme::mid(), theme::ink())
        };
        let mut cols = vec![
            Col::fixed((index + 1).to_string(), INDEX_COLS, theme::dim()),
            Col::fixed(glyph, GLYPH_COLS, glyph_style),
            Col::flex(kit::upper(&profile.title()), title_style),
        ];
        if is_active {
            cols.push(Col::auto("ACTIVE", theme::dim()));
        }
        let mark = if index == cursor {
            RowMark::Cursor
        } else {
            RowMark::Plain
        };
        kit::list_row(f, Rect::new(inner.x, y, inner.width, 1), &cols, mark);
        y += 1;
        if y < rows_bottom {
            let indent = INDEX_COLS + GLYPH_COLS;
            let room = width.saturating_sub(indent as usize);
            let style = if index == cursor { theme::mid() } else { theme::dim() };
            f.buffer_mut().set_stringn(
                inner.x + indent,
                y,
                kit::clip(&profile.description(), room),
                room,
                style,
            );
        }
        y += if full { 2 } else { 1 };
    }

    if inner.height > 2 {
        let hints = [
            KeyHint::new("J K", "SELECT", 2),
            KeyHint::new("ENTER", "START MIX", 3),
            KeyHint::new("ESC", "CANCEL", 3),
        ];
        let line = kit::key_row(&hints, inner.width, None);
        f.buffer_mut().set_line(inner.x, bottom - 1, &line, inner.width);
    }
}

fn bold_ink() -> ratatui::style::Style {
    theme::ink().add_modifier(Modifier::BOLD)
}
