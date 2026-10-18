//! The Telemetry screen chrome around the tab bodies: header, tab row, mini player strip and key
//! row, and the rows each takes (style sheet 2a, screens 2b–2f, 80×24 screens 3f/3g).
//!
//! Everything here reads state already in memory (the clock through `ClockCache`), so drawing
//! it every frame never touches the disk or the network.

use ratatui::prelude::*;

use super::kit::{self, KeyHint};
use super::screen::{Screen, WidthTier};
use super::{format_time, theme, AppUI};

/// Columns left blank at each side of the screen.
const GUTTER: u16 = 2;

/// Columns between the parts of the header, of the strip, and between the key hints and the
/// status message.
const PART_GAP: usize = 4;

/// Columns between two tabs. Each tab is also padded by one column on each side, so inactive
/// labels sit four columns apart, as in the mockups.
const TAB_GAP: usize = 2;

/// Priority of the hints a key row never drops (`? KEYS`, and `N QUEUE` on Now Playing).
const TOP: u8 = 9;

/// Keys shown at most in the 80–89 column tier.
const COMPACT_KEYS: usize = 5;

/// A status message needs at least this many columns to share the Now Playing key row with the
/// volume meter; with less room the meter gives way.
const MIN_STATUS_COLS: usize = 12;

/// Tab labels from 110 columns.
const FULL_TAB_LABELS: [&str; 5] = ["NOW PLAYING", "QUEUE & LIBRARY", "DEVICES", "SETTINGS", "PLAYLISTS"];

/// Tab labels from 90 to 109 columns.
const MEDIUM_TAB_LABELS: [&str; 5] = ["NOW PLAYING", "QUEUE", "DEVICES", "SETTINGS", "PLAYLISTS"];

/// Tab labels under 90 columns, and the active tab's label in the numbers-only fallback.
const SHORT_TAB_LABELS: [&str; 5] = ["NOW", "QUEUE", "DEVICES", "SETTINGS", "PLAYLISTS"];

/// Where each part of the chrome goes in one frame. Every rect already leaves out the gutters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChromeLayout {
    /// `None` under 24 rows, where the header folds into the tab row.
    pub header: Option<Rect>,
    pub tabs: Rect,
    pub content: Rect,
    /// The rule above the strip; `None` with the strip.
    pub rule: Option<Rect>,
    /// The mini player strip; `None` on Now Playing and when there is no room.
    pub strip: Option<Rect>,
    pub keys: Rect,
}

impl ChromeLayout {
    /// Rows top to bottom. 24 rows and taller: margin, header, blank, tab row, a gap (2 rows
    /// from 30 rows, else 1), content, blank, rule, strip, blank, key row, margin; without the
    /// strip, the rule, strip and one blank row go. Under 24 rows: tab row, content, strip, key
    /// row.
    pub fn new(screen: &Screen, with_strip: bool) -> Self {
        let area = screen.area;
        let gutter = if area.width > 2 * GUTTER { GUTTER } else { 0 };
        let row = |y: u16, height: u16| Rect::new(area.x + gutter, y, area.width - 2 * gutter, height);
        let line = |offset: u16| row(area.y + offset, 1);
        let h = area.height;

        if !screen.short {
            let gap = if h >= 30 { 2 } else { 1 };
            let top = 4 + gap;
            let bottom = if with_strip { 6 } else { 3 };
            return Self {
                header: Some(line(1)),
                tabs: line(3),
                content: row(area.y + top, h - top - bottom),
                rule: with_strip.then(|| line(h - 5)),
                strip: with_strip.then(|| line(h - 4)),
                keys: line(h - 2),
            };
        }

        let empty = row(area.y, 0);
        let keys = if h >= 2 { line(h - 1) } else { empty };
        let strip = (with_strip && h >= 3).then(|| line(h - 2));
        let bottom = u16::from(h >= 2) + u16::from(strip.is_some());
        Self {
            header: None,
            tabs: if h >= 1 { line(0) } else { empty },
            content: row(area.y + h.min(1), h.saturating_sub(1 + bottom)),
            rule: None,
            strip,
            keys,
        }
    }
}

/// Tab labels (`1 NOW PLAYING`, …) for a terminal `width` columns wide: the label set of its
/// width tier, else the short set, else (issue #59) only the numbers plus the active tab's short
/// label, so the active tab is always shown whole.
pub(super) fn tab_titles(width: u16, active: usize) -> Vec<String> {
    let room = usize::from(width.saturating_sub(2 * GUTTER));
    let numbered = |labels: &[&str; 5]| -> Vec<String> {
        labels
            .iter()
            .enumerate()
            .map(|(i, label)| format!("{} {}", i + 1, label))
            .collect()
    };
    let tier_labels = match WidthTier::from_width(width) {
        WidthTier::Full => &FULL_TAB_LABELS,
        WidthTier::Medium => &MEDIUM_TAB_LABELS,
        _ => &SHORT_TAB_LABELS,
    };
    for labels in [tier_labels, &SHORT_TAB_LABELS] {
        let titles = numbered(labels);
        if tab_row_width(&titles) <= room {
            return titles;
        }
    }
    SHORT_TAB_LABELS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            if i == active {
                format!("{} {}", i + 1, label)
            } else {
                (i + 1).to_string()
            }
        })
        .collect()
}

/// Columns the tab row takes: each title padded by one column on each side, `TAB_GAP` apart.
fn tab_row_width(titles: &[String]) -> usize {
    titles.iter().map(|t| Span::raw(t.as_str()).width() + 2).sum::<usize>() + TAB_GAP * titles.len().saturating_sub(1)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `left` at the start of a `width`-column line and `right` against its end, with spaces between.
fn spread<'a>(mut left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: usize) -> Line<'a> {
    let used = spans_width(&left) + spans_width(&right);
    if used < width {
        left.push(Span::raw(" ".repeat(width - used)));
    }
    left.extend(right);
    Line::from(left)
}

impl AppUI {
    /// The output the header names: `LOCAL/MPV`, or `DLNA/<RENDERER>`.
    fn output_label(&self) -> String {
        match self.device_manager.get_active_device() {
            Some(device) => format!("DLNA/{}", kit::upper(&device.friendly_name)),
            None => format!("LOCAL/{}", kit::upper(&self.settings_mgr.settings.player_backend)),
        }
    }

    /// The header's right-hand part (output, `WEB :port`, clock, 4 columns apart) in at most
    /// `room` columns: `WEB` goes first, then the output, then the clock.
    fn header_status(&mut self, room: usize) -> Vec<Span<'static>> {
        let clock = Span::styled(self.clock.get().to_string(), theme::ink());
        let output = Span::styled(self.output_label(), theme::mid());
        let web = self
            .web_server
            .as_ref()
            .map(|server| Span::styled(format!("WEB :{}", server.port), theme::mid()));
        let candidates = [
            vec![Some(output.clone()), web, Some(clock.clone())],
            vec![Some(output), Some(clock.clone())],
            vec![Some(clock)],
        ];
        for parts in candidates {
            let mut spans = Vec::new();
            for part in parts.into_iter().flatten() {
                if !spans.is_empty() {
                    spans.push(Span::raw(" ".repeat(PART_GAP)));
                }
                spans.push(part);
            }
            if spans_width(&spans) <= room {
                return spans;
            }
        }
        Vec::new()
    }

    /// `QUICKSTREAM ▪ <version>` on the left, output, web server and clock on the right.
    pub(super) fn render_header(&mut self, f: &mut Frame, area: Rect) {
        let left = vec![
            Span::styled("QUICKSTREAM", theme::ink().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ▪ {}", env!("CARGO_PKG_VERSION")), theme::dim()),
        ];
        let width = usize::from(area.width);
        let room = width.saturating_sub(spans_width(&left) + PART_GAP);
        let right = self.header_status(room);
        f.render_widget(spread(left, right, width), area);
    }

    /// The tabs, the active one as a reverse-video chip. With `with_status` (short terminals,
    /// which have no header row) the header's right-hand part follows at the right end.
    pub(super) fn render_tab_row(&mut self, f: &mut Frame, area: Rect, screen: &Screen, with_status: bool) {
        let titles = tab_titles(screen.area.width, self.active_tab);
        let mut left = Vec::new();
        for (i, title) in titles.iter().enumerate() {
            if i > 0 {
                left.push(Span::raw(" ".repeat(TAB_GAP)));
            }
            let style = if i == self.active_tab {
                theme::cursor()
            } else {
                theme::mid()
            };
            left.push(Span::styled(format!(" {title} "), style));
        }
        let width = usize::from(area.width);
        let right = if with_status {
            let room = width.saturating_sub(tab_row_width(&titles) + PART_GAP);
            self.header_status(room)
        } else {
            Vec::new()
        };
        f.render_widget(spread(left, right, width), area);
    }

    /// The rule above the strip.
    pub(super) fn render_rule(&self, f: &mut Frame, area: Rect) {
        let rule = Span::styled("─".repeat(usize::from(area.width)), theme::rule());
        f.render_widget(Line::from(rule), area);
    }

    /// The mini player strip (tabs 2–5): state and title (and artist from 90 columns) on the
    /// left; elapsed time, progress bar, duration and volume on the right.
    pub(super) fn render_strip(&self, f: &mut Frame, area: Rect, screen: &Screen) {
        let width = usize::from(area.width);
        let vol = self.player.volume();
        let vol_gap = if screen.tier >= WidthTier::Medium { PART_GAP } else { 2 };
        let vol_number = Span::styled(vol.to_string(), theme::ink().add_modifier(Modifier::BOLD));

        let Some(track) = self.queue.current_track() else {
            let left = vec![Span::styled("■ NO TRACK LOADED", theme::dim())];
            let right = vec![Span::styled("VOL ", theme::mid()), vol_number];
            f.render_widget(spread(left, right, width), area);
            return;
        };

        // A debounced skip has moved the queue but not the player yet: show the target track
        // from its start, marked as queued.
        let glyph = if self.pending_track_skip {
            "»"
        } else if self.player.is_playing() {
            "▶"
        } else if self.player.is_paused() {
            "‖"
        } else {
            "■"
        };
        let (pos, mut dur) = if self.pending_track_skip {
            (0.0, track.duration_seconds)
        } else {
            let pos = self
                .pending_seek_target
                .unwrap_or_else(|| self.player.position_seconds());
            (pos, self.player.duration_seconds())
        };
        if dur <= 0.0 {
            dur = track.duration_seconds;
        }
        let ratio = if dur > 0.0 { pos / dur } else { 0.0 };
        let (pos, dur) = (format_time(pos), format_time(dur));

        let title = kit::upper(track.display_title());
        let title_min = 2 + Span::raw(title.as_str()).width().min(12);
        let mut right: Vec<Span> = Vec::new();
        if self.pending_track_skip {
            right.push(Span::styled(format!("QUEUED{}", " ".repeat(PART_GAP)), theme::dim()));
        }
        let volume = [
            Span::styled(format!("{}VOL ", " ".repeat(vol_gap)), theme::mid()),
            vol_number,
        ];
        // Bar width from the mockups: 42 at 120 columns, 26 at 80. It shrinks to leave the title
        // some room, and gives way to `pos / dur` when it would be too short to read.
        let fixed = spans_width(&right) + 6 + 2 + 6 + spans_width(&volume);
        let inner = (width * 2 / 5)
            .saturating_sub(4)
            .min(width.saturating_sub(title_min + PART_GAP + fixed));
        if inner >= 6 {
            right.push(Span::styled(format!("{pos} "), theme::mid()));
            right.extend(kit::progress_bar(inner, ratio));
            right.push(Span::styled(format!(" {dur}"), theme::mid()));
            right.extend(volume);
        } else {
            right.push(Span::styled(format!("{pos} / {dur}"), theme::mid()));
            if spans_width(&right) + spans_width(&volume) + title_min + PART_GAP <= width {
                right.extend(volume);
            }
        }
        if spans_width(&right) + title_min + PART_GAP > width {
            right.clear();
        }

        let room = width.saturating_sub(spans_width(&right) + if right.is_empty() { 0 } else { PART_GAP });
        let title = kit::clip(&title, room.saturating_sub(2));
        let mut left = vec![Span::styled(
            format!("{glyph} {title}"),
            theme::ink().add_modifier(Modifier::BOLD),
        )];
        let artist_room = room.saturating_sub(spans_width(&left) + 3);
        if screen.tier >= WidthTier::Medium && !track.artist.is_empty() && artist_room >= 6 {
            let artist = kit::clip(&kit::upper(&track.artist), artist_room);
            left.push(Span::styled(format!("   {artist}"), theme::mid()));
        }
        f.render_widget(spread(left, right, width), area);
    }

    /// The status message for the key row, or `None` when it only repeats what the screen
    /// already shows. Errors are drawn in the ERROR colour.
    fn status_span(&self) -> Option<Span<'static>> {
        let message = self.status_message.as_str();
        let redundant = message.is_empty()
            || message.starts_with("Playing:")
            || message == "Playing"
            || message == "Paused"
            || message == "Stopped"
            || message.starts_with("Welcome to Quickstream");
        if redundant {
            return None;
        }
        let lower = message.to_lowercase();
        let style = if ["error", "failed", "couldn't"].iter().any(|w| lower.contains(w)) {
            theme::error()
        } else {
            theme::mid()
        };
        Some(Span::styled(message.to_string(), style))
    }

    /// The keys that do something on the active tab (and pane) right now, most important with
    /// the highest priority. `? KEYS` is always among the top ones, except while a tab is taking
    /// typed text (spotlight, startup home editor), where `?` is typed rather than opening help.
    pub fn tab_key_hints(&self) -> Vec<KeyHint> {
        let help = || KeyHint::new("?", "KEYS", TOP);
        match self.active_tab {
            0 => {
                let play = if self.player.is_playing() { "PAUSE" } else { "PLAY" };
                let step = self.settings_mgr.settings.seek_step_seconds;
                vec![
                    KeyHint::new("SPC", play, 5),
                    KeyHint::new("←→", "TRACK", 4),
                    KeyHint::new("[ ]", format!("SEEK {step}S"), 3),
                    KeyHint::new("+−", "VOL", 2),
                    // #57: `n` (never Esc, the quit key) leaves Now Playing, at every width.
                    KeyHint::new("N", "QUEUE", TOP),
                    help(),
                ]
            }
            1 if self.browser_spotlight.is_some() => vec![
                KeyHint::new("↑↓", "MATCH", 7),
                KeyHint::new("ENTER", "OPEN / ADD", 8),
                KeyHint::new("ESC", "CLOSE", TOP),
            ],
            1 if self.queue_focus => vec![
                KeyHint::new("H L", "PANE", 7),
                KeyHint::new("ENTER", "PLAY", 8),
                KeyHint::new("D", "REMOVE", 5),
                KeyHint::new("C", "CLEAR", 3),
                KeyHint::new("F", "SEARCH", 6),
                KeyHint::new("V", "TAGS/FILES", 4),
                help(),
            ],
            1 => {
                let mut hints = vec![KeyHint::new("H L", "PANE", 7), KeyHint::new("ENTER", "OPEN / ADD", 8)];
                // `a` adds the folder's tracks; Music Homes has none.
                if !self.in_bookmarks_view {
                    hints.push(KeyHint::new("A", "ADD ALL", 4));
                }
                hints.extend([
                    KeyHint::new("/", "SPOTLIGHT", 5),
                    KeyHint::new("F", "SEARCH", 6),
                    KeyHint::new("V", "TAGS/FILES", 3),
                    help(),
                ]);
                hints
            }
            2 => vec![
                KeyHint::new("J K", "SELECT", 6),
                KeyHint::new("ENTER", "USE OUTPUT", 8),
                KeyHint::new("R", "RESCAN", 7),
                help(),
            ],
            3 if self.editing_home_dir.is_some() => {
                vec![KeyHint::new("ENTER", "SAVE", TOP), KeyHint::new("ESC", "CANCEL", TOP)]
            }
            3 => {
                // Enter on Startup Home Directory opens its editor; on the rest it cycles.
                let enter = if self.settings_cursor == 7 { "EDIT" } else { "CHANGE" };
                vec![
                    KeyHint::new("J K", "SELECT", 6),
                    KeyHint::new("← →", "CHANGE", 8),
                    KeyHint::new("ENTER", enter, 7),
                    help(),
                ]
            }
            4 if self.playlists.tracks_focus => vec![
                KeyHint::new("H L", "PANE", 7),
                KeyHint::new("ENTER", "PLAY", 8),
                KeyHint::new("⇧J ⇧K", "MOVE TRACK", 5),
                KeyHint::new("D", "REMOVE", 6),
                KeyHint::new("A", "QUEUE ALL", 4),
                help(),
            ],
            4 => vec![
                KeyHint::new("H L", "PANE", 7),
                KeyHint::new("ENTER", "OPEN", 8),
                KeyHint::new("A", "QUEUE", 5),
                KeyHint::new("C ⇧R ⇧D", "NEW / RENAME / DELETE", 6),
                help(),
            ],
            _ => vec![help()],
        }
    }

    /// The key row: the tab's keys from the left, the status message (and on Now Playing the
    /// volume meter) at the right. A long status takes the room of the least important keys.
    pub(super) fn render_key_row(&self, f: &mut Frame, area: Rect, screen: &Screen) {
        let mut hints = self.tab_key_hints();
        if screen.tier == WidthTier::Compact {
            hints = kit::top_hints(&hints, COMPACT_KEYS);
        }
        let width = usize::from(area.width);
        let meter = (self.active_tab == 0).then(|| {
            let vol = self.player.volume();
            if screen.tier >= WidthTier::Medium {
                kit::volume_meter(vol)
            } else {
                vec![
                    Span::styled("VOL ", theme::dim()),
                    Span::styled(vol.to_string(), theme::ink().add_modifier(Modifier::BOLD)),
                ]
            }
        });

        let right = match (self.status_span(), meter) {
            (None, meter) => meter,
            (Some(status), None) => Some(vec![status]),
            (Some(status), Some(meter)) => {
                // Room left of the meter once only the never-dropped keys remain.
                let room = width
                    .saturating_sub(kit::top_priority_width(&hints) + PART_GAP)
                    .saturating_sub(spans_width(&meter) + PART_GAP);
                if room >= MIN_STATUS_COLS {
                    let text = kit::clip(&status.content, room);
                    let mut spans = vec![Span::styled(text, status.style), Span::raw(" ".repeat(PART_GAP))];
                    spans.extend(meter);
                    Some(spans)
                } else {
                    Some(vec![status])
                }
            }
        };
        f.render_widget(kit::key_row(&hints, area.width, right), area);
    }
}
