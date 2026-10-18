//! Small drawing pieces shared by the Telemetry screens: key row, volume meter, progress bar,
//! display-width-aware text fitting and a cached clock.
//!
//! Everything here is pure (no I/O) and cheap enough to call on every frame.

use std::time::{Duration, Instant};

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::theme;

/// Columns between two key hints, and between the hints and the right-hand part of a key row.
const KEY_GAP: usize = 4;

/// Cells in the volume meter.
const VOLUME_CELLS: usize = 11;

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
fn text_width(text: &str) -> usize {
    Span::raw(text).width()
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
    let right_width = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();

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
    while left + gap(left, right_width(&right)) + right_width(&right) > width {
        match next_to_drop(hints, &kept, true) {
            Some(i) => {
                kept[i] = false;
                left = hints_width(&kept);
            }
            None => break,
        }
    }
    let room = width.saturating_sub(left + gap(left, 1));
    if right_width(&right) > room {
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
    let used = left + right_width(&right);
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
        let mut text = String::new();
        for c in span.content.chars() {
            let w = char_width(c);
            if used + w > budget {
                break;
            }
            used += w;
            text.push(c);
        }
        text.push('…');
        out.push(Span::styled(text, span.style));
        return out;
    }
    out
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
