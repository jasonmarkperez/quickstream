//! The fuzzy search modal (`f`, mockup 2h): the query row with its counts, RANKED MATCHES and,
//! from 90 columns, a TRACK panel with the selected track's fields.
//!
//! Only what the modal already holds is drawn (the streamed tracks' metadata); nothing is read
//! from disk, and only the visible rows are laid out.

use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::kit::{self, Col, KeyHint, ModalSpec, PanelSpec, RowMark};
use super::now_playing::{channels_label, khz};
use super::queue_tab::display_path;
use super::search::SearchModalState;
use super::theme;
use crate::metadata::AudioMetadata;

/// The modal's largest size: 100 columns as mockup 2h, and 32 rows as the help, which leaves the
/// tab row and key row in view at 120x40. It shrinks to leave 2 columns and 2 rows of the screen
/// around it, but never under [`MIN_HEIGHT`] rows.
const MAX_WIDTH: u16 = 100;
const MAX_HEIGHT: u16 = 32;

/// The fewest rows the modal takes (shrunk to the screen when shorter): the compact frame's
/// borders and the query row.
const MIN_HEIGHT: u16 = 3;

/// Screens this tall get the full frame (title row, padding, the rule under the query); shorter
/// ones the compact frame.
const FULL_MIN_ROWS: u16 = 28;

/// Screens this wide get the TRACK panel beside the matches.
const DETAIL_MIN_COLS: u16 = 90;

/// Columns between RANKED MATCHES and TRACK.
const PANEL_GAP: u16 = 4;

/// The TRACK panel's label column (`DURATION` and a space).
const DETAIL_LABEL_COLS: usize = 10;

/// Rows a panel header takes (label, rule, blank row), and the fewest list rows the matches keep
/// with their header. With less room (short terminals) the matches go right under the query row,
/// with no blank row, no RANKED MATCHES header and no TRACK panel.
const PANEL_HEADER_ROWS: u16 = 3;
const MIN_LIST_ROWS: u16 = 3;

/// What the TRACK panel drops first when it is short of rows, most expendable first.
const DETAIL_DROP_ORDER: [&str; 4] = ["SIZE", "CHANNELS", "BITRATE", "RATE"];

/// Draw the search modal centred on the screen.
pub(super) fn render(f: &mut Frame, modal: &mut SearchModalState, home: Option<&Path>) {
    let screen = f.area();
    let full = screen.height >= FULL_MIN_ROWS;
    // At least the compact frame and the query row, however short the screen.
    let height = screen.height.saturating_sub(4).clamp(MIN_HEIGHT, MAX_HEIGHT);
    let area = kit::modal_area(screen, MAX_WIDTH, height);
    let root = display_path(&modal.root_dir, home);
    let note = modal.is_scanning.then(|| {
        let elapsed = modal.scan_start.map(|start| start.elapsed()).unwrap_or_default();
        Line::from(Span::styled(
            format!("SCANNING {}", kit::spinner(elapsed)),
            theme::mid(),
        ))
    });
    let inner = kit::modal_frame(
        f,
        area,
        ModalSpec {
            title: "SEARCH",
            subtitle: Some(&root),
            note,
            error: false,
            compact: !full,
        },
    );
    if inner.is_empty() {
        return;
    }

    let mut y = inner.y;
    draw_query_row(f, Rect::new(inner.x, y, inner.width, 1), modal);
    y += 1;
    if full && y < inner.bottom() {
        let width = inner.width as usize;
        f.buffer_mut()
            .set_stringn(inner.x, y, "─".repeat(width), width, theme::ink());
        y += 1;
    }
    // A blank row, then the panels down to the footer (after a blank row in the full layout).
    let footer_rows = if full { 2 } else { 1 };
    let bottom = inner.bottom().saturating_sub(footer_rows);
    let room = bottom.saturating_sub(y);
    if room >= 1 + PANEL_HEADER_ROWS + MIN_LIST_ROWS {
        let panels = Rect::new(inner.x, y + 1, inner.width, room - 1);
        let (list, detail) = split(panels, screen.width >= DETAIL_MIN_COLS);
        draw_matches(f, list, modal);
        if let Some(detail) = detail {
            draw_detail(f, detail, modal.selected_track(), home);
        }
    } else if room > 0 {
        // Too short for the headers: the matches alone, right under the query row.
        let width = inner.width.saturating_sub(kit::PANEL_RIGHT_PAD);
        draw_match_rows(f, Rect::new(inner.x, y, width, room), modal);
    }
    if inner.height > footer_rows {
        draw_footer(
            f,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            modal.filtered.len(),
        );
    }
}

/// RANKED MATCHES and, when `detail`, TRACK beside it (56 : 36 of the 92 columns in 2h, gap
/// included).
fn split(area: Rect, detail: bool) -> (Rect, Option<Rect>) {
    if !detail || area.width <= PANEL_GAP * 4 {
        return (area, None);
    }
    let left = (area.width as u32 * 52 / 92) as u16;
    let right = area.width - left - PANEL_GAP;
    (
        Rect { width: left, ..area },
        Some(Rect::new(area.x + left + PANEL_GAP, area.y, right, area.height)),
    )
}

/// `QUERY  halc█` on the left (the query as typed, bold, with a block cursor; its start cut with
/// `…` so the cursor stays visible) and `4 / 4,812 MATCHED` on the right.
fn draw_query_row(f: &mut Frame, area: Rect, modal: &SearchModalState) {
    let width = area.width as usize;
    let counts = vec![
        Span::styled(grouped(modal.filtered.len()), theme::bold()),
        Span::styled(format!(" / {} MATCHED", grouped(modal.all_tracks.len())), theme::dim()),
    ];
    let counts_width = kit::spans_width(&counts);
    let label = "QUERY  ";
    // The counts only while the query keeps room for a few characters beside them.
    let show_counts = label.len() + 8 + 2 + counts_width <= width;
    let query_room =
        width
            .saturating_sub(label.len() + 1)
            .saturating_sub(if show_counts { counts_width + 2 } else { 0 });
    let query = kit::visible_tail(&modal.query, query_room);
    let cursor = Style::new().bg(theme::palette().ink);
    let line = Line::from(vec![
        Span::styled(label, theme::dim()),
        Span::styled(query, theme::bold()),
        Span::styled(" ", cursor),
    ]);
    let buf = f.buffer_mut();
    buf.set_line(area.x, area.y, &line, area.width);
    if show_counts {
        let x = area.right() - counts_width as u16;
        buf.set_line(x, area.y, &Line::from(counts), counts_width as u16);
    }
}

/// `4812` as `4,812`.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The RANKED MATCHES panel: `TITLE  ARTIST  05:52` rows (only the visible ones), the cursor bar
/// on the selection and the query's matched characters underlined.
fn draw_matches(f: &mut Frame, area: Rect, modal: &mut SearchModalState) {
    let content = kit::panel(
        f,
        area,
        PanelSpec {
            label: "RANKED MATCHES",
            focused: true,
            ..Default::default()
        },
    );
    draw_match_rows(f, content, modal);
}

/// The match rows in `content` (or `No tracks match your query.`).
fn draw_match_rows(f: &mut Frame, content: Rect, modal: &mut SearchModalState) {
    if content.is_empty() {
        return;
    }
    if modal.filtered.is_empty() {
        let width = content.width as usize;
        f.buffer_mut().set_stringn(
            content.x,
            content.y,
            kit::clip("No tracks match your query.", width),
            width,
            theme::dim(),
        );
        return;
    }

    // The length column fits the longest length in the whole list, so it doesn't change width
    // while scrolling (worked out once per list, as each row's underlined characters are).
    let length_cols: u16 = if modal.long_lengths() { 7 } else { 5 };
    let window = kit::visible_window(&mut modal.list_state, modal.filtered.len(), content.height as usize);
    for (row, index) in window.enumerate() {
        let (title_marks, artist_marks) = modal.row_marks(index);
        let track = &modal.filtered[index].1;
        let title = kit::upper(track.display_title());
        let artist = kit::upper(&track.artist);
        let length = if track.duration_seconds > 0.0 {
            format!(
                "{:>width$}",
                kit::duration(track.duration_seconds),
                width = length_cols as usize
            )
        } else {
            String::new()
        };
        let cols = [
            Col::flex(title, theme::ink()).underline_chars(title_marks),
            Col::flex(artist, theme::mid()).underline_chars(artist_marks),
            Col::fixed(length, length_cols, theme::mid()),
        ];
        let mark = if index == modal.selected_index {
            RowMark::Cursor
        } else {
            RowMark::Plain
        };
        let y = content.y + row as u16;
        kit::list_row(f, Rect::new(content.x, y, content.width, 1), &cols, mark);
    }
}

/// The TRACK panel: the selected track's fields that have data (`TITLE`, `ARTIST`, `ALBUM`,
/// `DURATION`, `FORMAT`, `RATE`, `BITRATE`, `CHANNELS`, `SIZE`), then its path under `PATH`, in
/// its own case with `~` for home, wrapped at `/`. Short of rows, `SIZE`, `CHANNELS`, `BITRATE`
/// and `RATE` go first, then the start of the path.
fn draw_detail(f: &mut Frame, area: Rect, track: Option<&AudioMetadata>, home: Option<&Path>) {
    let content = kit::panel(
        f,
        area,
        PanelSpec {
            label: "TRACK",
            focused: true,
            ..Default::default()
        },
    );
    let Some(track) = track else { return };
    if content.is_empty() {
        return;
    }
    let width = content.width as usize;
    let rows = content.height as usize;
    let mut fields = detail_fields(track);
    let path = display_path(&track.filepath, home);
    let mut path_lines = wrap_path(&path, width);

    // Fit the rows: blank + PATH + the path's lines under the fields.
    let path_rows = |lines: &Vec<String>| if lines.is_empty() { 0 } else { lines.len() + 2 };
    while fields.len() + path_rows(&path_lines) > rows {
        if let Some(i) = DETAIL_DROP_ORDER
            .iter()
            .find_map(|label| fields.iter().position(|(l, _)| l == label))
        {
            fields.remove(i);
        } else if path_lines.len() > 1 {
            let dropped = path_lines.remove(0);
            path_lines[0] = kit::visible_tail(&format!("{dropped}{}", path_lines[0]), width);
        } else if !path_lines.is_empty() {
            path_lines.clear();
        } else {
            fields.truncate(rows);
        }
    }

    let buf = f.buffer_mut();
    let value_room = width.saturating_sub(DETAIL_LABEL_COLS);
    let mut y = content.y;
    for (label, value) in &fields {
        buf.set_stringn(content.x, y, label, width, theme::dim());
        if value_room > 0 {
            let style = if *label == "TITLE" { theme::bold() } else { theme::ink() };
            buf.set_stringn(
                content.x + DETAIL_LABEL_COLS as u16,
                y,
                kit::clip(value, value_room),
                value_room,
                style,
            );
        }
        y += 1;
    }
    if !path_lines.is_empty() {
        y += 1;
        buf.set_stringn(content.x, y, "PATH", width, theme::dim());
        y += 1;
        for line in &path_lines {
            buf.set_stringn(content.x, y, line, width, theme::mid());
            y += 1;
        }
    }
}

/// The TRACK panel's fields that have data, in the 2h order.
fn detail_fields(track: &AudioMetadata) -> Vec<(&'static str, String)> {
    let mut fields = vec![("TITLE", kit::upper(track.display_title()))];
    if !track.artist.is_empty() {
        fields.push(("ARTIST", kit::upper(&track.artist)));
    }
    if !track.album.is_empty() {
        fields.push(("ALBUM", kit::upper(&track.album)));
    }
    if track.duration_seconds > 0.0 {
        fields.push(("DURATION", kit::duration(track.duration_seconds)));
    }
    if !track.format_name.is_empty() {
        fields.push(("FORMAT", kit::upper(&track.format_name)));
    }
    if track.sample_rate_hz > 0 {
        fields.push(("RATE", format!("{} KHZ", khz(track.sample_rate_hz))));
    }
    if track.bitrate_kbps > 0 {
        fields.push(("BITRATE", format!("{} KBPS", track.bitrate_kbps)));
    }
    if track.channels > 0 {
        fields.push(("CHANNELS", channels_label(track.channels)));
    }
    if track.file_size > 0 {
        fields.push(("SIZE", format!("{:.1} MB", track.file_size as f64 / 1_048_576.0)));
    }
    fields
}

/// `path` in lines of at most `width` columns, broken after a `/` where it can be and anywhere
/// in a name longer than a line.
fn wrap_path(path: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for part in path.split_inclusive('/') {
        let part_width = kit::text_width(part);
        if used + part_width > width && used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for c in part.chars() {
            let w = kit::char_width(c);
            if used + w > width && used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += w;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// ` ENTER  ADD & CLOSE    TAB  ADD TO QUEUE    ⇧TAB  ADD ALL (4)    ESC  CANCEL`; too narrow
/// for all of them, `⇧TAB`, `TAB`, then `ENTER` go, so `ESC  CANCEL` always shows whole. The
/// screen's key row shows the same keys, plus `↑↓ SELECT`.
pub(super) fn footer_hints(matches: usize) -> Vec<KeyHint> {
    vec![
        KeyHint::new("ENTER", "ADD & CLOSE", 3),
        KeyHint::new("TAB", "ADD TO QUEUE", 2),
        KeyHint::new("⇧TAB", format!("ADD ALL ({})", grouped(matches)), 1),
        KeyHint::new("ESC", "CANCEL", 4),
    ]
}

/// [`footer_hints`] in `area`.
fn draw_footer(f: &mut Frame, area: Rect, matches: usize) {
    let line = kit::key_row(&footer_hints(matches), area.width, None);
    f.buffer_mut().set_line(area.x, area.y, &line, area.width);
}
