//! Small drawing pieces shared by the Telemetry screens: key row, volume meter, progress bar,
//! big numerals, panel headers, list rows in columns, display-width-aware text fitting and a
//! cached clock.
//!
//! Everything here is pure (no I/O) and cheap enough to call on every frame.

use std::borrow::Cow;
use std::ops::Range;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::ListState;
use ratatui::Frame;

use super::theme;

/// Columns between two key hints, and between the hints and the right-hand part of a key row.
const KEY_GAP: usize = 4;

/// Cells in the volume meter.
const VOLUME_CELLS: usize = 11;

/// Columns at the right of a panel kept clear of text: the rule's ` ▪` sits there, and the cursor
/// bar reaches one column into it.
pub(super) const PANEL_RIGHT_PAD: u16 = 2;

/// Columns between a panel label and its right note.
const LABEL_GAP: usize = 2;

/// How much of a label is kept, at least, before its right note is dropped instead.
const LABEL_MIN_KEPT: usize = 8;

/// One entry of a key row: a keycap (` SPC `) followed by what it does (` PAUSE`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyHint {
    pub keys: String,
    pub label: String,
    /// Higher is more important. When the row is too narrow, the lowest goes first; the hints
    /// sharing the highest priority in a row are never dropped.
    pub priority: u8,
}

impl KeyHint {
    pub fn new(keys: impl Into<String>, label: impl Into<String>, priority: u8) -> Self {
        Self {
            keys: keys.into(),
            label: label.into(),
            priority,
        }
    }

    /// Columns the hint takes: ` KEYS ` plus ` LABEL`.
    fn width(&self) -> usize {
        text_width(&self.keys) + 2 + 1 + text_width(&self.label)
    }
}

/// Columns `text` takes in the terminal.
pub(super) fn text_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Columns `spans` take together in the terminal.
pub(super) fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// The longest start of `text` that fits in `budget` columns, and the columns it takes. A wide
/// character that would straddle the cut is left out.
fn cut(text: &str, budget: usize) -> (String, usize) {
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    (out, used)
}

/// Index of the hint to drop next among those still `kept`: the lowest priority, the rightmost
/// of equals. `protect_top` keeps the hints with the highest priority in `hints`.
fn next_to_drop(hints: &[KeyHint], kept: &[bool], protect_top: bool) -> Option<usize> {
    let top = hints.iter().map(|h| h.priority).max()?;
    hints
        .iter()
        .enumerate()
        .filter(|&(i, h)| kept[i] && !(protect_top && h.priority == top))
        .min_by_key(|&(i, h)| (h.priority, std::cmp::Reverse(i)))
        .map(|(i, _)| i)
}

/// The `max` most important of `hints`, in their original order (for the 80–89 column tier,
/// which shows at most 5 keys).
pub fn top_hints(hints: &[KeyHint], max: usize) -> Vec<KeyHint> {
    let mut kept = vec![true; hints.len()];
    let mut count = hints.len();
    while count > max {
        match next_to_drop(hints, &kept, false) {
            Some(i) => {
                kept[i] = false;
                count -= 1;
            }
            None => break,
        }
    }
    hints
        .iter()
        .zip(&kept)
        .filter(|(_, &k)| k)
        .map(|(h, _)| h.clone())
        .collect()
}

/// Columns the hints with the highest priority take with their gaps: the narrowest a key row of
/// `hints` gets, since those are never dropped.
pub fn top_priority_width(hints: &[KeyHint]) -> usize {
    let Some(top) = hints.iter().map(|h| h.priority).max() else {
        return 0;
    };
    let widths: Vec<usize> = hints.iter().filter(|h| h.priority == top).map(KeyHint::width).collect();
    widths.iter().sum::<usize>() + KEY_GAP * widths.len().saturating_sub(1)
}

/// A key row `width` columns wide: the hints from the left, 4 columns apart, and `right` (a status
/// message or the volume meter) against the right edge.
///
/// While the row does not fit, the lowest-priority hint is dropped (the order of the rest is
/// kept). The hints with the highest priority always stay; if they and `right` still do not fit,
/// `right` is cut with `…`.
pub fn key_row<'a>(hints: &[KeyHint], width: u16, right: Option<Vec<Span<'a>>>) -> Line<'a> {
    let width = width as usize;
    let mut right = right.unwrap_or_default();

    let mut kept = vec![true; hints.len()];
    let hints_width = |kept: &[bool]| {
        let widths: Vec<usize> = hints
            .iter()
            .zip(kept)
            .filter(|(_, &k)| k)
            .map(|(h, _)| h.width())
            .collect();
        widths.iter().sum::<usize>() + KEY_GAP * widths.len().saturating_sub(1)
    };
    let gap = |left: usize, right: usize| if left > 0 && right > 0 { KEY_GAP } else { 0 };

    let mut left = hints_width(&kept);
    while left + gap(left, spans_width(&right)) + spans_width(&right) > width {
        match next_to_drop(hints, &kept, true) {
            Some(i) => {
                kept[i] = false;
                left = hints_width(&kept);
            }
            None => break,
        }
    }
    let room = width.saturating_sub(left + gap(left, 1));
    if spans_width(&right) > room {
        right = truncate_spans(right, room);
    }

    let mut spans: Vec<Span<'a>> = Vec::new();
    for (hint, _) in hints.iter().zip(&kept).filter(|(_, &k)| k) {
        if !spans.is_empty() {
            spans.push(Span::raw(" ".repeat(KEY_GAP)));
        }
        spans.push(Span::styled(format!(" {} ", hint.keys), theme::keycap()));
        spans.push(Span::styled(format!(" {}", hint.label), theme::mid()));
    }
    let used = left + spans_width(&right);
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans.extend(right);
    Line::from(spans)
}

/// `spans` cut to at most `width` columns, ending in `…` (in the style of the span it cuts).
fn truncate_spans(spans: Vec<Span<'_>>, width: usize) -> Vec<Span<'_>> {
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1; // the `…`
    let mut used = 0;
    let mut out = Vec::new();
    for span in spans {
        let span_width = span.width();
        if used + span_width <= budget {
            used += span_width;
            out.push(span);
            continue;
        }
        let (mut text, _) = cut(&span.content, budget - used);
        text.push('…');
        out.push(Span::styled(text, span.style));
        return out;
    }
    out
}

/// Blank `area` for a modal drawn over the screen: every cell is reset and set to INK on GROUND.
/// Use this instead of ratatui's `Clear`, which leaves the cells on the terminal's default
/// background.
pub fn clear(buf: &mut Buffer, area: Rect) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].reset();
        }
    }
    buf.set_style(area, theme::base());
}

/// `VOL ▮▮▮▮▮▮▮▯▯▯▯ 65`: 11 cells, filled in INK, empty in DIM, the number in bold INK.
pub fn volume_meter(vol: u8) -> Vec<Span<'static>> {
    let filled = (vol.min(100) as usize * VOLUME_CELLS + 50) / 100;
    let mut spans = vec![Span::styled("VOL ", theme::dim())];
    if filled > 0 {
        spans.push(Span::styled("▮".repeat(filled), theme::ink()));
    }
    if filled < VOLUME_CELLS {
        spans.push(Span::styled("▯".repeat(VOLUME_CELLS - filled), theme::dim()));
    }
    spans.push(Span::styled(
        format!(" {vol}"),
        theme::ink().add_modifier(Modifier::BOLD),
    ));
    spans
}

/// `▕╱╱╱╱╱╱▏` with `inner_width` slashes: the elapsed share (`ratio`, clamped to 0–1) in INK, the
/// rest in RULE.
pub fn progress_bar(inner_width: usize, ratio: f64) -> Vec<Span<'static>> {
    let ratio = if ratio.is_nan() { 0.0 } else { ratio.clamp(0.0, 1.0) };
    let elapsed = ((ratio * inner_width as f64).round() as usize).min(inner_width);
    let mut spans = vec![Span::styled("▕", theme::ink())];
    if elapsed > 0 {
        spans.push(Span::styled("╱".repeat(elapsed), theme::ink()));
    }
    if elapsed < inner_width {
        spans.push(Span::styled("╱".repeat(inner_width - elapsed), theme::rule()));
    }
    spans.push(Span::styled("▏", theme::ink()));
    spans
}

/// Rows of the big numerals `0`–`9` (3 columns each, heavy box drawing; style sheet 2a).
const BIG_DIGITS: [[&str; 3]; 10] = [
    ["┏━┓", "┃ ┃", "┗━┛"],
    ["  ╻", "  ┃", "  ╹"],
    ["╺━┓", "┏━┛", "┗━╸"],
    ["╺━┓", " ━┫", "╺━┛"],
    ["╻ ╻", "┗━┫", "  ╹"],
    ["┏━╸", "┗━┓", "╺━┛"],
    ["┏━╸", "┣━┓", "┗━┛"],
    ["╺━┓", "  ┃", "  ╹"],
    ["┏━┓", "┣━┫", "┗━┛"],
    ["┏━┓", "┗━┫", "╺━┛"],
];

/// The big-numeral glyph of `c`: a digit (3 columns) or `:` (1 column, the dot on the middle row).
fn big_glyph(c: char) -> Option<[&'static str; 3]> {
    match c {
        ':' => Some([" ", ":", " "]),
        _ => c.to_digit(10).map(|d| BIG_DIGITS[d as usize]),
    }
}

/// `text` (a time such as `02:41` or `1:15:00`) in the 3-row big numerals of Now Playing, top row
/// first, glyphs one column apart. Characters other than digits and `:` are left out.
pub fn big_digits(text: &str) -> [String; 3] {
    let mut rows: [String; 3] = Default::default();
    for (n, glyph) in text.chars().filter_map(big_glyph).enumerate() {
        for (row, part) in rows.iter_mut().zip(glyph) {
            if n > 0 {
                row.push(' ');
            }
            row.push_str(part);
        }
    }
    rows
}

/// Columns each row of [`big_digits`]`(text)` takes.
pub fn big_digits_width(text: &str) -> usize {
    let (count, cols) = text.chars().filter_map(big_glyph).fold((0, 0), |(count, cols), glyph| {
        (count + 1, cols + glyph[0].chars().count())
    });
    (cols + count).saturating_sub(1)
}

/// What [`panel`] draws above a panel's content.
#[derive(Clone, Debug, Default)]
pub struct PanelSpec<'a> {
    /// Shown in UPPERCASE at the left of the label row.
    pub label: &'a str,
    /// A short note at the right end of the label row (UPPERCASE, DIM), e.g. `TAGS` or `LOOP`.
    pub right: Option<&'a str>,
    /// The panel has keyboard focus: label and rule in INK.
    pub focused: bool,
    /// A DIM row under the rule (a path, a position, the spotlight query). Its text is drawn as
    /// given, so paths keep their case; span styles are applied over DIM.
    pub footnote: Option<Line<'a>>,
    /// Narrow (80–89 columns) or short terminals: the footnote goes to the right end of the label
    /// row, in place of the right note, and takes no row of its own.
    pub compact: bool,
}

/// Draw a panel header in `area` and return the area left for the content.
///
/// Rows: the label row (label, right note), the rule (`─` then ` ▪`), the footnote if any (not
/// when compact), a blank row, then the content. Focused: label bold INK, rule and `▪` INK.
/// Unfocused: label MID, rule RULE, `▪` DIM. No borders. The returned area is the text area:
/// it stops 2 columns short of `area`'s right edge, under the ` ▪`, so text lines up with the
/// right note and a cursor bar ([`list_row`]) can reach one column past it on each side.
pub fn panel(f: &mut Frame, area: Rect, spec: PanelSpec) -> Rect {
    let area = area.intersection(f.area());
    let buf = f.buffer_mut();
    let text_width = area.width.saturating_sub(PANEL_RIGHT_PAD);
    let mut y = area.y;
    let bottom = area.bottom();

    let footnote_row = !spec.compact && spec.footnote.is_some();
    let right: Option<Vec<Span>> = if spec.compact && spec.footnote.is_some() {
        spec.footnote.as_ref().map(|line| styled_over(line, theme::dim()))
    } else {
        spec.right.map(|note| vec![Span::styled(upper(note), theme::dim())])
    };

    if y < bottom {
        draw_label_row(
            buf,
            Rect::new(area.x, y, text_width, 1),
            spec.label,
            spec.focused,
            right,
        );
        y += 1;
    }
    if y < bottom {
        let (rule_style, tick_style) = if spec.focused {
            (theme::ink(), theme::ink())
        } else {
            (theme::rule(), theme::dim())
        };
        buf.set_stringn(
            area.x,
            y,
            "─".repeat(text_width as usize),
            text_width as usize,
            rule_style,
        );
        buf.set_stringn(
            area.x + text_width,
            y,
            " ▪",
            (area.width - text_width) as usize,
            tick_style,
        );
        y += 1;
    }
    if footnote_row && y < bottom {
        if let Some(line) = &spec.footnote {
            let spans = styled_over(line, theme::dim());
            let spans = if spans_width(&spans) > text_width as usize {
                truncate_spans(spans, text_width as usize)
            } else {
                spans
            };
            buf.set_line(area.x, y, &Line::from(spans), text_width);
        }
        y += 1;
    }
    // The blank row before the content.
    y = (y + 1).min(bottom);
    Rect::new(area.x, y, text_width, bottom - y)
}

/// `line`'s spans with each span's style applied over `base`.
fn styled_over<'a>(line: &Line<'a>, base: Style) -> Vec<Span<'a>> {
    line.spans
        .iter()
        .map(|span| Span::styled(span.content.clone(), base.patch(line.style).patch(span.style)))
        .collect()
}

/// The label at the left of `area` and `right` against its right edge. When both do not fit,
/// the label is cut to make room for `right` while it keeps at least [`LABEL_MIN_KEPT`] columns;
/// otherwise `right` is dropped and the label cut to the row.
fn draw_label_row(buf: &mut Buffer, area: Rect, label: &str, focused: bool, right: Option<Vec<Span>>) {
    let width = area.width as usize;
    let label = upper(label);
    let label_width = text_width(&label);
    let right = right.filter(|spans| {
        let right_width = spans_width(spans);
        right_width > 0 && right_width + LABEL_GAP + label_width.min(LABEL_MIN_KEPT) <= width
    });
    let label_room = match &right {
        Some(spans) => width - spans_width(spans) - LABEL_GAP,
        None => width,
    };
    buf.set_stringn(
        area.x,
        area.y,
        clip(&label, label_room),
        label_room,
        theme::label(focused),
    );
    if let Some(spans) = right {
        let right_width = spans_width(&spans) as u16;
        buf.set_line(area.right() - right_width, area.y, &Line::from(spans), right_width);
    }
}

/// The rows of a `len`-row list shown `height` rows tall, scrolled the way a ratatui `List` of
/// one-row items scrolls `state`: the offset moves only as far as needed to show the selected
/// row, is clamped to the last row, and is written back to `state`. A selection past the end
/// moves to the last row; an empty list clears it. A zero height leaves `state` alone.
pub fn visible_window(state: &mut ListState, len: usize, height: usize) -> Range<usize> {
    if height == 0 {
        return 0..0;
    }
    if len == 0 {
        state.select(None);
        return 0..0;
    }
    if state.selected().is_some_and(|s| s >= len) {
        state.select(Some(len - 1));
    }
    let offset = state.offset().min(len - 1);
    let mut first = offset;
    let mut last = (offset + height).min(len);
    let shown = state.selected().unwrap_or(offset);
    if shown >= last {
        last = shown + 1;
        first = first.max(last.saturating_sub(height));
    }
    if shown < first {
        first = shown;
        last = last.min(first + height);
    }
    *state.offset_mut() = first;
    first..last
}

/// How wide a [`Col`] of a list row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColWidth {
    /// Exactly this many columns.
    Fixed(u16),
    /// Whatever the other columns leave (shared equally if there are several).
    Flex,
    /// The text's own width, e.g. a duration at the right end after a [`ColWidth::Flex`] column.
    Auto,
}

/// One column of a list row.
#[derive(Clone, Debug)]
pub struct Col<'a> {
    pub text: Cow<'a, str>,
    pub width: ColWidth,
    pub style: Style,
    /// Characters of `text` (char indices) drawn underlined, e.g. a spotlight match.
    pub underline: Option<Range<usize>>,
}

impl<'a> Col<'a> {
    pub fn fixed(text: impl Into<Cow<'a, str>>, width: u16, style: Style) -> Self {
        Self {
            text: text.into(),
            width: ColWidth::Fixed(width),
            style,
            underline: None,
        }
    }

    pub fn flex(text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        Self {
            text: text.into(),
            width: ColWidth::Flex,
            style,
            underline: None,
        }
    }

    pub fn auto(text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        Self {
            text: text.into(),
            width: ColWidth::Auto,
            style,
            underline: None,
        }
    }

    /// Underline the characters `range` (char indices into the text). Characters cut off by the
    /// column's `…` are not drawn, so neither is their underline.
    pub fn underline(mut self, range: Range<usize>) -> Self {
        self.underline = Some(range);
        self
    }
}

/// How a list row shows that it is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMark {
    /// Not selected.
    Plain,
    /// Selected in the focused pane: the cursor bar ([`theme::cursor`]), one column wider than
    /// the row's area on each side.
    Cursor,
    /// Selected in an unfocused pane: `▸` is drawn in the first column, in that column's style.
    /// Callers keep that column for it (a marker column with no text of its own, the same on
    /// every row), so it never hides what the row shows, such as a playing glyph.
    Marker,
}

/// Draw one list row on the first line of `area`, its columns left to right.
///
/// Each column but the last is cut (with `…`, by display width, so CJK text stays aligned) to
/// leave at least one space before the next. Flex columns share what the fixed and auto columns
/// leave; when even those do not fit, the flex columns get nothing and the row is cut at the
/// right edge of `area`.
pub fn list_row(f: &mut Frame, area: Rect, cols: &[Col], mark: RowMark) {
    let screen = f.area();
    let area = Rect::new(area.x, area.y, area.width, area.height.min(1)).intersection(screen);
    if area.is_empty() {
        return;
    }
    let buf = f.buffer_mut();
    let widths = column_widths(cols, area.width as usize);
    let mut x = area.x;
    for (i, (col, width)) in cols.iter().zip(widths).enumerate() {
        let width = width.min((area.right() - x) as usize);
        if width == 0 {
            continue;
        }
        let text = if i == 0 && mark == RowMark::Marker {
            "▸"
        } else {
            col.text.as_ref()
        };
        let is_last = i + 1 == cols.len();
        let budget = if is_last || width < 2 { width } else { width - 1 };
        let shown = if is_last || width < 2 {
            fit(text, width)
        } else {
            format!("{} ", fit(text, width - 1))
        };
        buf.set_stringn(x, area.y, shown, width, col.style);
        if let (Some(range), false) = (&col.underline, i == 0 && mark == RowMark::Marker) {
            underline_chars(buf, x, area.y, text, budget, range);
        }
        x += width as u16;
    }
    if mark == RowMark::Cursor {
        let left = area.x.saturating_sub(1);
        let right = (area.right() + 1).min(screen.right());
        buf.set_style(Rect::new(left, area.y, right - left, 1), theme::cursor());
    }
}

/// Underline, in the row at (`x`, `y`), the characters `range` of `text` as [`fit`] draws it in
/// `budget` columns: only the characters kept before a `…`.
fn underline_chars(buf: &mut Buffer, x: u16, y: u16, text: &str, budget: usize, range: &Range<usize>) {
    let kept = if text_width(text) <= budget {
        usize::MAX
    } else {
        cut(text, budget.saturating_sub(1)).0.chars().count()
    };
    let mut cx = x;
    for (i, c) in text.chars().enumerate() {
        if i >= kept || i >= range.end {
            break;
        }
        let w = char_width(c) as u16;
        if i >= range.start {
            for dx in 0..w.max(1) {
                if let Some(cell) = buf.cell_mut((cx + dx, y)) {
                    cell.modifier.insert(Modifier::UNDERLINED);
                }
            }
        }
        cx += w;
    }
}

/// Widths of `cols` laid out in `width` columns: fixed as given, auto as wide as their text (plus
/// the one-column gap when not last), flex sharing the rest.
fn column_widths(cols: &[Col], width: usize) -> Vec<usize> {
    let last = cols.len().saturating_sub(1);
    let mut widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, col)| match col.width {
            ColWidth::Fixed(n) => n as usize,
            ColWidth::Auto => text_width(&col.text) + usize::from(i != last),
            ColWidth::Flex => 0,
        })
        .collect();
    let flex: Vec<usize> = (0..cols.len()).filter(|&i| cols[i].width == ColWidth::Flex).collect();
    if !flex.is_empty() {
        let rest = width.saturating_sub(widths.iter().sum());
        for (n, &i) in flex.iter().enumerate() {
            widths[i] = rest / flex.len() + usize::from(n < rest % flex.len());
        }
    }
    widths
}

/// A length or a position as the TUI shows it everywhere (list rows, totals, the strip, Now
/// Playing): `MM:SS` under an hour (`04:02`), `H:MM:SS` from an hour (`1:15:00`). Rounded to
/// the second; negative values read `00:00`.
pub fn duration(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Uppercase with full Unicode case mapping (`ß` becomes `SS`, so the result can be longer).
pub fn upper(text: &str) -> String {
    text.to_uppercase()
}

/// `text` padded with spaces or cut with `…` to exactly `width` columns. A wide character that
/// would straddle the cut is left out and the gap padded, so columns stay aligned.
pub fn fit(text: &str, width: usize) -> String {
    let text_cols = text_width(text);
    if text_cols <= width {
        return format!("{text}{}", " ".repeat(width - text_cols));
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1; // the `…`
    let (mut out, used) = cut(text, budget);
    out.push('…');
    out.push_str(&" ".repeat(budget - used));
    out
}

/// `text` if it fits in `width` columns, else cut to at most `width` columns ending in `…`
/// (no padding, unlike [`fit`]).
pub fn clip(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    fit(text, width).trim_end_matches(' ').to_string()
}

/// Columns `c` takes in the terminal (2 for wide characters such as CJK).
pub fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// `text` if it fits in `width` columns; otherwise `…` followed by as much of its end as fits.
pub fn visible_tail(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    let mut used = 1; // the `…`
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        used += char_width(c);
        if used > width {
            break;
        }
        start = index;
    }
    format!("…{}", &text[start..])
}

/// The header clock (`HH:MM`, local time), read at most once per second however often it is
/// drawn.
pub struct ClockCache {
    text: String,
    refreshed: Option<Instant>,
    source: fn() -> String,
}

impl ClockCache {
    const REFRESH: Duration = Duration::from_secs(1);

    pub fn new() -> Self {
        Self::with_source(local_hh_mm)
    }

    /// A clock that reads its text from `source` (tests pass a fake).
    pub fn with_source(source: fn() -> String) -> Self {
        Self {
            text: String::new(),
            refreshed: None,
            source,
        }
    }

    pub fn get(&mut self) -> &str {
        self.get_at(Instant::now())
    }

    /// The clock text as of `now`, read again only if the last read is a second or more old.
    pub fn get_at(&mut self, now: Instant) -> &str {
        if self.refreshed.is_none_or(|at| now.duration_since(at) >= Self::REFRESH) {
            self.text = (self.source)();
            self.refreshed = Some(now);
        }
        &self.text
    }
}

impl Default for ClockCache {
    fn default() -> Self {
        Self::new()
    }
}

fn local_hh_mm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}
