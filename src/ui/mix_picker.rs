//! The autopilot mix picker (`M`, mockup 2i): one row per profile the picker offers
//! ([`MixProfile::picker_profiles`]), its text taken from the profile itself (title, description
//! with the BPM range from the code), `●` and `ACTIVE` on the profile in use and `○` on the others.
//! A profile in use that the picker doesn't list (Seed Radio, Focus) marks no row; the title row
//! names it instead (`NOW: SEED RADIO`).

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::ListState;
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
/// the profiles. Shorter screens drop the subtitle, its rule and the blank rows too, and scroll
/// the profiles to keep the cursor's in view.
const COMPACT_HEIGHT: u16 = 2 + 3 + 5 * 2 + 2;

/// Columns of the index (`1`) and of the `●`/`○` before a profile's title; its description lines
/// up with the title.
const INDEX_COLS: u16 = 3;
const GLYPH_COLS: u16 = 3;

/// Rows a profile takes: its title row and its description.
const PROFILE_ROWS: u16 = 2;

/// The picker's footer keys; the screen's key row shows the same ones, plus `1-5 PROFILE`. Too
/// narrow for every key, `J K` then `ENTER` go, so `ESC  CANCEL` always shows whole.
pub(super) fn footer_hints() -> Vec<KeyHint> {
    vec![
        KeyHint::new("J K", "SELECT", 2),
        KeyHint::new("ENTER", "START MIX", 3),
        KeyHint::new("ESC", "CANCEL", 4),
    ]
}

/// Draw the mix picker centred on the screen; `cursor` is the highlighted row, `active` the
/// profile in use and `list` the scroll position when not every profile fits.
pub(super) fn render(f: &mut Frame, cursor: usize, active: &MixProfile, list: &mut ListState) {
    let screen = f.area();
    let full = screen.height >= FULL_HEIGHT;
    let tight = !full && screen.height < COMPACT_HEIGHT;
    let area = kit::modal_area(screen, WIDTH, if full { FULL_HEIGHT } else { COMPACT_HEIGHT });
    // Only the profile's own row is marked; one the picker doesn't list is named in the title row.
    let active_row = active.picker_row();
    let note = active_row
        .is_none()
        .then(|| Line::from(format!("NOW: {}", kit::upper(active.short_name()))));
    let inner = kit::modal_frame(
        f,
        area,
        ModalSpec {
            title: "AUTOPILOT MIX",
            note,
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

    if !tight {
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
    }

    // The footer takes the last row (after a blank one unless tight).
    let footer_rows = if tight { 1 } else { 2 };
    let rows_bottom = bottom.saturating_sub(footer_rows);
    // As many whole profiles as fit (at least the cursor's title), scrolled to the cursor.
    let gap = if full { 1 } else { 0 };
    let list_rows = rows_bottom.saturating_sub(y);
    let fit = ((list_rows + gap) / (PROFILE_ROWS + gap)).max(list_rows.min(1));
    let profiles = MixProfile::picker_profiles();
    list.select(Some(cursor));
    let window = kit::visible_window(list, profiles.len(), fit as usize);
    for index in window {
        let profile = &profiles[index];
        if y >= rows_bottom {
            break;
        }
        let is_active = active_row == Some(index);
        let (glyph, glyph_style, title_style) = if is_active {
            ("●", theme::bold(), theme::bold())
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
        y += PROFILE_ROWS - 1 + gap;
    }

    if inner.height > footer_rows {
        let line = kit::whole_key_row(&footer_hints(), inner.width);
        f.buffer_mut().set_line(inner.x, bottom - 1, &line, inner.width);
    }
}
