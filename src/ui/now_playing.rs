//! Tab 1 Now Playing in the Telemetry style (mockups 2b at 120×40, 3f at 80×24).
//!
//! From the top: the NOW PLAYING panel (position, queue mode and player state in its footnote,
//! the elapsed time in big numerals, then title, artist, album and year) with the SIGNAL panel
//! beside it from 110 columns; the progress bar across the whole width; elapsed and remaining
//! time with the file's path; then UP NEXT. Under 110 columns SIGNAL becomes a one-line tech
//! summary under the bar and the title moves beside the numerals. When rows run short, UP NEXT
//! goes first, then the elapsed row.
//!
//! Only data already in memory is drawn: the queue's tags and the features the features worker
//! has found for the current track ([`TrackFeaturesWorker::features_for`]). A field without data
//! is left out, never made up (no bit depth: only the sample rate is known). The Up Next count,
//! total length and column widths are cached per [`PlaybackQueue::revision`], so a frame never
//! walks the queue.
//!
//! [`TrackFeaturesWorker::features_for`]: super::features::TrackFeaturesWorker::features_for
//! [`PlaybackQueue::revision`]: crate::queue::PlaybackQueue::revision

use ratatui::prelude::*;

use super::kit::{self, text_width, Col, PanelSpec, RowMark};
use super::queue_tab::{display_path, duration_cols, queue_mode_note, right_aligned, SEP};
use super::screen::{Screen, WidthTier};
use super::{theme, AppUI};
use crate::analysis::TrackFeatures;
use crate::metadata::AudioMetadata;
use crate::queue::{QueueEnd, Upcoming};

/// Width of the SIGNAL panel (from 110 columns) and the gap between it and NOW PLAYING (2b).
const SIGNAL_COLS: u16 = 34;
const PANEL_GAP: u16 = 4;

/// NOW PLAYING keeps at least this many columns beside SIGNAL, or SIGNAL is not drawn.
const MIN_HERO_COLS: u16 = 40;

/// Width of a SIGNAL row's label column.
const SIGNAL_LABEL_COLS: u16 = 10;

/// Columns between the numerals and the title beside them (under 110 columns, 3f).
const TITLE_GAP: u16 = 5;

/// Separator of the artist / album / year line.
const META_SEP: &str = "  /  ";

/// Separator of the tech line's fields.
const TECH_SEP: &str = "  ";

/// Columns between the bar and the length after it (under 110 columns).
const BAR_GAP: usize = 4;

/// Columns the percentage after the bar takes (` 100%`, from 110 columns).
const PERCENT_COLS: usize = 5;

/// Up Next rows wanted, at least, before the elapsed row gets a row of its own.
const MIN_UP_NEXT_ROWS: u16 = 3;

/// Rows of a panel header without a footnote: label, rule, blank.
const PLAIN_HEADER_ROWS: u16 = 3;

/// Shown under NO TRACK LOADED.
const EMPTY_HINT: &str =
    "Press Space to play active queue, or navigate to [2] Queue & Library to browse your collection.";

/// Formats whose bitrate says more than their sample rate in the Up Next format column.
const LOSSY_FORMATS: [&str; 7] = ["mp3", "aac", "m4a", "mp4", "ogg", "opus", "wma"];

/// Containers that hold either a lossy codec (AAC) or a lossless one (ALAC). The codec isn't in
/// the metadata, so the bitrate decides: above this it is taken as lossless (AAC tops out
/// around 320 kbps; ALAC at 44.1 kHz runs about 700–1000).
const LOSSY_OR_LOSSLESS: [&str; 2] = ["m4a", "mp4"];
const LOSSLESS_MIN_KBPS: u32 = 500;

/// What the Up Next summary was built for: the queue revision, the queue mode and whether the
/// UPnP continuous stream was playing.
type UpNextKey = (u64, String, bool);

/// Values for tab 1 that would otherwise need a pass over the queue every frame.
#[derive(Debug, Default)]
pub(super) struct NowPlayingCache {
    /// The Up Next summary and what it was built for.
    up_next: Option<(UpNextKey, UpNextSummary)>,
    /// How many times the summary has been built (tests check it is not rebuilt every frame).
    builds: u64,
}

/// The tracks that play after the current one: which, their total length and what the columns
/// need.
#[derive(Debug, Clone, Copy)]
struct UpNextSummary {
    /// The tracks in play order ([`crate::queue::PlaybackQueue::upcoming`]).
    upcoming: Upcoming,
    remaining: usize,
    /// `None` when nothing remains or any length is unknown.
    total: Option<f64>,
    any_artist: bool,
    duration_cols: usize,
    /// Widest format badge (`FLAC 96`, `MP3 320`).
    format_cols: usize,
}

/// A sample rate in kHz: `96`, `44.1`.
fn khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        (hz / 1000).to_string()
    } else {
        format!("{:.1}", f64::from(hz) / 1000.0)
    }
}

fn channels_label(channels: u8) -> String {
    match channels {
        1 => "MONO".to_string(),
        2 => "STEREO".to_string(),
        n => format!("{n} CH"),
    }
}

/// The Up Next format column: the format and its sample rate in kHz (`FLAC 96`, and an M4A or
/// MP4 above [`LOSSLESS_MIN_KBPS`], likely ALAC), or its bitrate for lossy formats (`MP3 320`);
/// empty without a format.
fn format_badge(track: &AudioMetadata) -> String {
    if track.format_name.is_empty() {
        return String::new();
    }
    let format = kit::upper(&track.format_name);
    let name = track.format_name.to_lowercase();
    let lossless_bitrate = LOSSY_OR_LOSSLESS.contains(&name.as_str()) && track.bitrate_kbps > LOSSLESS_MIN_KBPS;
    let lossy = LOSSY_FORMATS.contains(&name.as_str()) && !lossless_bitrate;
    let detail = if lossy && track.bitrate_kbps > 0 {
        track.bitrate_kbps.to_string()
    } else if track.sample_rate_hz > 0 {
        khz(track.sample_rate_hz)
    } else if track.bitrate_kbps > 0 {
        track.bitrate_kbps.to_string()
    } else {
        return format;
    };
    format!("{format} {detail}")
}

/// The genre from the tags, else from the analysis.
fn genre(track: &AudioMetadata, features: Option<&TrackFeatures>) -> Option<String> {
    [Some(track.genre.as_str()), features.map(|f| f.genre.as_str())]
        .into_iter()
        .flatten()
        .find(|g| !g.trim().is_empty())
        .map(kit::upper)
}

fn bpm(features: Option<&TrackFeatures>) -> Option<String> {
    features
        .and_then(|f| f.bpm)
        .filter(|bpm| *bpm > 0.0)
        .map(|bpm| format!("{bpm:.0}"))
}

/// The Camelot key (`8A`), else the key as tagged.
fn key(features: Option<&TrackFeatures>) -> Option<String> {
    let features = features?;
    features
        .camelot_key
        .as_deref()
        .or(features.initial_key.as_deref())
        .filter(|k| !k.trim().is_empty())
        .map(kit::upper)
}

/// The SIGNAL rows that have data, in the 2b order.
fn signal_rows(track: &AudioMetadata, features: Option<&TrackFeatures>) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if !track.format_name.is_empty() {
        rows.push(("FORMAT", kit::upper(&track.format_name)));
    }
    if track.sample_rate_hz > 0 {
        rows.push(("RATE", format!("{} KHZ", khz(track.sample_rate_hz))));
    }
    if track.bitrate_kbps > 0 {
        rows.push(("BITRATE", format!("{} KBPS", track.bitrate_kbps)));
    }
    if track.channels > 0 {
        rows.push(("CHANNELS", channels_label(track.channels)));
    }
    if let Some(genre) = genre(track, features) {
        rows.push(("GENRE", genre));
    }
    if let Some(bpm) = bpm(features) {
        rows.push(("BPM", bpm));
    }
    if let Some(key) = key(features) {
        rows.push(("KEY", key));
    }
    if let Some(energy) = features.and_then(|f| f.energy) {
        rows.push(("ENERGY", format!("{energy:.2}")));
    }
    if track.file_size > 0 {
        rows.push(("SIZE", format!("{:.1} MB", track.file_size as f64 / 1_048_576.0)));
    }
    rows
}

/// SIGNAL labels from the most useful to the least: when the rows don't all fit, the last ones
/// here are cut first.
const SIGNAL_VALUE_ORDER: [&str; 9] = [
    "FORMAT", "RATE", "BPM", "KEY", "BITRATE", "GENRE", "ENERGY", "CHANNELS", "SIZE",
];

/// At most `n` of `rows`, keeping the most useful ([`SIGNAL_VALUE_ORDER`]) in their own order.
fn fit_signal_rows(rows: Vec<(&'static str, String)>, n: usize) -> Vec<(&'static str, String)> {
    if rows.len() <= n {
        return rows;
    }
    let rank = |label: &str| {
        SIGNAL_VALUE_ORDER
            .iter()
            .position(|l| *l == label)
            .unwrap_or(usize::MAX)
    };
    let mut ranks: Vec<usize> = rows.iter().map(|(label, _)| rank(label)).collect();
    ranks.sort_unstable();
    let cutoff = ranks.get(n.wrapping_sub(1)).copied();
    match cutoff {
        Some(cutoff) => rows.into_iter().filter(|(label, _)| rank(label) <= cutoff).collect(),
        None => Vec::new(),
    }
}

/// The fields of the tech line under the bar (under 110 columns, 3f), most important first.
fn tech_parts(track: &AudioMetadata, features: Option<&TrackFeatures>) -> Vec<String> {
    let mut parts = Vec::new();
    if !track.format_name.is_empty() {
        parts.push(kit::upper(&track.format_name));
    }
    if track.sample_rate_hz > 0 {
        parts.push(format!("{} KHZ", khz(track.sample_rate_hz)));
    }
    if track.bitrate_kbps > 0 {
        parts.push(format!("{} KBPS", track.bitrate_kbps));
    }
    if track.channels > 0 {
        parts.push(channels_label(track.channels));
    }
    if let Some(bpm) = bpm(features) {
        parts.push(format!("{bpm} BPM"));
    }
    parts.extend(key(features));
    parts
}

/// As many of `parts` as fit in `width` columns, joined with `sep`; the last ones go first. A
/// first part too wide on its own is cut with `…`.
fn joined_to_fit(parts: &[String], sep: &str, width: usize) -> String {
    for n in (1..=parts.len()).rev() {
        let joined = parts[..n].join(sep);
        if text_width(&joined) <= width {
            return joined;
        }
    }
    parts.first().map(|p| kit::clip(p, width)).unwrap_or_default()
}

/// The file name shown in Filenames mode, in its own case.
fn file_label(track: &AudioMetadata) -> String {
    if !track.filename.is_empty() {
        return track.filename.clone();
    }
    track
        .filepath
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Draw `line` on row `dy` of `area`, if `area` has that row.
fn line_at(f: &mut Frame, area: Rect, dy: u16, line: Line) {
    if dy >= area.height {
        return;
    }
    let row = Rect::new(area.x, area.y + dy, area.width, 1).intersection(f.area());
    if !row.is_empty() {
        f.render_widget(line, row);
    }
}

impl AppUI {
    /// How many times the Up Next summary (count, total length, columns) has been built.
    #[doc(hidden)]
    pub fn up_next_summary_builds(&self) -> u64 {
        self.now_playing.builds
    }

    /// The tracks that play after the current one, summed up once per queue revision (which also
    /// changes with the current index), queue mode and continuous-stream state.
    fn up_next_summary(&mut self) -> UpNextSummary {
        let (revision, wrap) = (self.queue.revision(), self.continuous_stream_active());
        let mode = &self.settings_mgr.settings.queue_mode;
        if let Some(((r, m, w), summary)) = &self.now_playing.up_next {
            if (*r, m, *w) == (revision, mode, wrap) {
                return *summary;
            }
        }
        let key: UpNextKey = (revision, mode.clone(), wrap);
        let upcoming = self.queue.upcoming(mode, wrap);
        let tracks = self.queue.tracks();
        let next: Vec<&AudioMetadata> = upcoming.map(|i| &tracks[i]).collect();
        let known = !next.is_empty() && next.iter().all(|t| t.duration_seconds > 0.0);
        let summary = UpNextSummary {
            upcoming,
            remaining: next.len(),
            total: known.then(|| next.iter().map(|t| t.duration_seconds).sum()),
            any_artist: next.iter().any(|t| !t.artist.is_empty()),
            duration_cols: duration_cols(next.iter().map(|t| t.duration_seconds).fold(0.0, f64::max)),
            format_cols: next.iter().map(|t| text_width(&format_badge(t))).max().unwrap_or(0),
        };
        self.now_playing.up_next = Some((key, summary));
        self.now_playing.builds += 1;
        summary
    }

    /// The player state as the footnote names it, with the strip's glyph
    /// ([`AppUI::player_state_glyph`]).
    fn state_word(&self) -> String {
        let word = match self.player_state_glyph() {
            "»" => "QUEUED",
            "▶" => "PLAYING",
            "‖" => "PAUSED",
            _ => "STOPPED",
        };
        format!("{} {word}", self.player_state_glyph())
    }

    /// The autopilot's mix profile (`DIVERSE SHUFFLE`) while it runs.
    fn autopilot_profile(&self) -> Option<String> {
        self.autopilot_dir
            .is_some()
            .then(|| kit::upper(self.active_mix_profile.short_name()))
    }

    /// The NOW PLAYING footnote: `TRACK 04/12 · QUEUE LOOP · ▶ PLAYING`. Compact (at the right
    /// end of the label row, in `room` columns): `04/12 · LOOP · ▶ PLAYING · ⚡ PROFILE`, giving
    /// up the mode, then the position, then the profile until it fits; the state always stays,
    /// as its glyph alone (`▶`) when even the word doesn't fit.
    fn hero_footnote(&self, has_track: bool, compact: bool, room: usize) -> String {
        let len = self.queue.len();
        let digits = len.to_string().len().max(2);
        let position = has_track.then(|| format!("{:0digits$}/{len:0digits$}", self.queue.current_index() + 1));
        let mode = queue_mode_note(&self.settings_mgr.settings.queue_mode);
        let state = self.state_word();
        if !compact {
            let mut parts: Vec<String> = position.into_iter().map(|p| format!("TRACK {p}")).collect();
            parts.push(format!("QUEUE {mode}"));
            parts.push(state);
            return parts.join(SEP);
        }
        let profile = self.autopilot_profile().map(|p| format!("⚡ {p}"));
        let candidates = [
            vec![position.clone(), Some(mode), Some(state.clone()), profile.clone()],
            vec![position, Some(state.clone()), profile.clone()],
            vec![Some(state.clone()), profile],
        ];
        for parts in candidates {
            let joined = parts.into_iter().flatten().collect::<Vec<_>>().join(SEP);
            if text_width(&joined) <= room {
                return joined;
            }
        }
        if text_width(&state) <= room {
            return state;
        }
        // Too narrow for the word: the glyph alone, which the label row keeps even when it has
        // to shorten the label.
        self.player_state_glyph().to_string()
    }

    /// The NOW PLAYING panel header; returns the area left for its content.
    fn hero_panel(&self, f: &mut Frame, area: Rect, has_track: bool, compact: bool) -> Rect {
        // The label, the 2 columns between it and the footnote, the 2 under the rule's ` ▪`.
        let room = usize::from(area.width).saturating_sub(text_width("NOW PLAYING") + 4);
        let footnote = self.hero_footnote(has_track, compact, room);
        let profile = self.autopilot_profile().map(|p| format!("AUTOPILOT · {p}"));
        kit::panel(
            f,
            area,
            PanelSpec {
                label: "Now Playing",
                right: profile.as_deref(),
                focused: true,
                footnote: Some(Line::from(footnote)),
                compact,
            },
        )
    }

    pub(super) fn render_now_playing_tab(&mut self, f: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let screen = Screen::new(f.area());
        let compact = screen.short || screen.tier <= WidthTier::Compact;
        let Some(track) = self.queue.current_track().cloned() else {
            let content = self.hero_panel(f, area, false, compact);
            let bold = theme::ink().add_modifier(Modifier::BOLD);
            line_at(f, content, 0, Line::styled("NO TRACK LOADED", bold));
            let hint = kit::clip(EMPTY_HINT, usize::from(content.width));
            line_at(f, content, 2, Line::styled(hint, theme::dim()));
            return;
        };
        let summary = self.up_next_summary();
        let features = self.track_features.features_for(&track.filepath);
        let full = screen.tier == WidthTier::Full && area.width >= SIGNAL_COLS + PANEL_GAP + MIN_HERO_COLS;

        let artist = (!track.artist.is_empty()).then(|| kit::upper(&track.artist));
        let album = (!track.album.is_empty()).then(|| kit::upper(&track.album));
        let year = features.and_then(|f| f.year).map(|y| y.to_string());
        let signal = if full {
            signal_rows(&track, features)
        } else {
            Vec::new()
        };
        let tech = if full { Vec::new() } else { tech_parts(&track, features) };

        // Rows, top to bottom: the panel header (label, rule, footnote unless compact, blank),
        // the body (numerals; from 110 columns also a blank, the title and the artist line, or
        // the SIGNAL rows when there are more), a blank, the bar, the tech line (under 110
        // columns), the elapsed row, then UP NEXT.
        // SIGNAL rows beyond the hero's own come last: the layout is decided with the hero's
        // rows alone (and without the features' year), so UP NEXT and the elapsed row never
        // depend on whether features have arrived, and SIGNAL gets the rows left over (its least
        // useful rows cut first).
        let header_rows: u16 = if compact { 3 } else { 4 };
        let height = area.height;
        let hero_body = if full {
            // The year comes with the features, so it doesn't count: a track without artist or
            // album shows it only when SIGNAL's rows have made the hero tall enough.
            let has_meta = artist.is_some() || album.is_some();
            5 + u16::from(has_meta)
        } else {
            3
        };
        let base_core = header_rows + hero_body + 2 + u16::from(!tech.is_empty());
        // UP NEXT's rows: up to MIN_UP_NEXT_ROWS tracks (or the end-of-queue note when none),
        // plus loop mode's `LOOPS TO 01` row when the list wraps among them.
        let listed = summary.remaining.min(usize::from(MIN_UP_NEXT_ROWS));
        let wraps = summary.upcoming.take(listed).any(|idx| idx == 0);
        let wanted = (listed.max(1) + usize::from(wraps)) as u16;
        let up_next_rows = 1 + PLAIN_HEADER_ROWS + wanted;
        let show_up_next = !screen.short && base_core + up_next_rows <= height;
        let show_info = base_core + 1 + if show_up_next { up_next_rows } else { 0 } <= height;
        let used = base_core + u16::from(show_info) + if show_up_next { up_next_rows } else { 0 };
        let spare = height.saturating_sub(used);
        let body = hero_body.max((signal.len() as u16).min(hero_body + spare));
        let signal = fit_signal_rows(signal, usize::from(body));
        let top = header_rows + body;
        let core = top + 2 + u16::from(!tech.is_empty());

        let (pos, dur) = self.playback_times();
        let elapsed = kit::duration(pos);
        let numerals = kit::big_digits(&elapsed);
        let bold = theme::ink().add_modifier(Modifier::BOLD);
        let title = kit::upper(track.display_title());

        let hero_width = if full {
            area.width - SIGNAL_COLS - PANEL_GAP
        } else {
            area.width
        };
        let hero = self.hero_panel(f, Rect::new(area.x, area.y, hero_width, top.min(height)), true, compact);
        // From 110 columns `/ 05:52` (regular DIM) follows the numerals' bottom row. Each row is
        // drawn once, as spans: a line style would leave BOLD on the whole row.
        let total = dur.filter(|_| full).map(|d| format!(" / {}", kit::duration(d)));
        for (dy, row) in numerals.iter().enumerate() {
            let mut spans = vec![Span::styled(row.clone(), bold)];
            if dy == 2 {
                spans.extend(total.as_ref().map(|t| Span::styled(t.clone(), theme::dim())));
            }
            line_at(f, hero, dy as u16, Line::from(spans));
        }
        let hero_cols = usize::from(hero.width);
        if full {
            // The title and the artist line below the numerals.
            line_at(f, hero, 4, Line::styled(kit::clip(&title, hero_cols), bold));
            let meta: Vec<String> = [artist, album, year].into_iter().flatten().collect();
            let meta = kit::clip(&meta.join(META_SEP), hero_cols);
            line_at(f, hero, 5, Line::styled(meta, theme::mid()));
            self.render_signal_panel(
                f,
                Rect::new(area.x + hero_width + PANEL_GAP, area.y, SIGNAL_COLS, top.min(height)),
                &signal,
                compact,
            );
        } else {
            // Title, artist, and album with year beside the numerals (3f).
            let x = hero.x + kit::big_digits_width(&elapsed) as u16 + TITLE_GAP;
            let beside = Rect::new(x, hero.y, hero.right().saturating_sub(x), hero.height);
            let album_year: Vec<String> = [album, year].into_iter().flatten().collect();
            let mut lines = vec![(title, bold)];
            lines.extend(artist.map(|a| (a, theme::mid())));
            if !album_year.is_empty() {
                lines.push((album_year.join(META_SEP), theme::mid()));
            }
            for (dy, (text, style)) in lines.into_iter().enumerate() {
                let text = kit::clip(&text, usize::from(beside.width));
                line_at(f, beside, dy as u16, Line::styled(text, style));
            }
        }

        // The bar across the whole width (to the panels' text edge, under their ` ▪`), then the
        // tech line and the elapsed row.
        let rows = Rect {
            width: area.width.saturating_sub(kit::PANEL_RIGHT_PAD),
            ..area
        };
        let width = usize::from(rows.width);
        let ratio = dur.map_or(0.0, |d| pos / d);
        let (after, reserve) = if full {
            let percent = dur.map(|_| format!("{}%", (ratio.clamp(0.0, 1.0) * 100.0).round() as u32));
            (
                percent.map(|p| Span::styled(right_aligned(&p, PERCENT_COLS), bold)),
                PERCENT_COLS,
            )
        } else {
            let total = dur.map(kit::duration);
            let reserve = total.as_ref().map_or(0, |t| BAR_GAP + text_width(t));
            (
                total.map(|t| Span::styled(right_aligned(&t, reserve), theme::mid())),
                reserve,
            )
        };
        let mut bar = kit::progress_bar(width.saturating_sub(2 + reserve), ratio);
        bar.extend(after);
        line_at(f, rows, top + 1, Line::from(bar));
        if !tech.is_empty() {
            let line = joined_to_fit(&tech, TECH_SEP, width);
            line_at(f, rows, top + 2, Line::styled(line, theme::dim()));
        }
        if show_info {
            let mut info = format!("ELAPSED {elapsed}");
            if let Some(dur) = dur {
                info.push_str(&format!("   REMAIN {}", kit::duration((dur - pos).max(0.0))));
            }
            let path = display_path(&track.filepath, self.queue_tab.home.as_deref());
            let room = width.saturating_sub(text_width(&info) + 3);
            if room >= 8 {
                info.push_str(&format!("   {}", kit::visible_tail(&path, room)));
            }
            line_at(f, rows, core, Line::styled(info, theme::dim()));
        }

        if show_up_next {
            let hero_rows = core + u16::from(show_info);
            let rest = height - hero_rows;
            let gap = if !compact && rest >= 2 + PLAIN_HEADER_ROWS + wanted {
                2
            } else {
                1
            };
            let up_next = Rect::new(area.x, area.y + hero_rows + gap, area.width, rest - gap);
            self.render_up_next(f, up_next, &screen, summary, full, compact);
        }
    }

    /// The SIGNAL panel (from 110 columns): `SOURCE <output>` in the footnote, then a row per
    /// field that has data.
    fn render_signal_panel(&self, f: &mut Frame, area: Rect, rows: &[(&'static str, String)], compact: bool) {
        let source = format!("SOURCE {}", self.output_label());
        let list = kit::panel(
            f,
            area,
            PanelSpec {
                label: "Signal",
                right: None,
                focused: true,
                footnote: Some(Line::from(source)),
                compact,
            },
        );
        for (dy, (label, value)) in rows.iter().enumerate().take(usize::from(list.height)) {
            let cols = [
                Col::fixed(*label, SIGNAL_LABEL_COLS, theme::dim()),
                Col::flex(value.as_str(), theme::ink()),
            ];
            let row = Rect::new(list.x, list.y + dy as u16, list.width, 1);
            kit::list_row(f, row, &cols, RowMark::Plain);
        }
    }

    /// UP NEXT: `n REMAINING · total` on the label row (the total only when every length is
    /// known, and not when compact), then the tracks after the current one: index, title,
    /// artist (from 80 columns, Tags mode), format (from 110 columns) and length.
    fn render_up_next(
        &self,
        f: &mut Frame,
        area: Rect,
        screen: &Screen,
        summary: UpNextSummary,
        full: bool,
        compact: bool,
    ) {
        let mut note = format!("{} REMAINING", summary.remaining);
        if let (Some(total), false) = (summary.total, compact) {
            note.push_str(SEP);
            note.push_str(&kit::duration(total));
        }
        let list = kit::panel(
            f,
            area,
            PanelSpec {
                label: "Up Next",
                right: Some(&note),
                focused: true,
                footnote: None,
                compact: false,
            },
        );
        let tracks = self.queue.tracks();
        let digits = tracks.len().to_string().len().max(2);
        let end = summary.upcoming.end();
        let width = usize::from(list.width);
        let note_row = |f: &mut Frame, dy: usize, text: &str| {
            line_at(f, list, dy as u16, Line::styled(kit::clip(text, width), theme::dim()));
        };
        if summary.remaining == 0 {
            // What plays when this track ends (`PlaybackQueue::upcoming`).
            let text = match end {
                QueueEnd::Stops => format!("END OF QUEUE{SEP}PLAYBACK STOPS AFTER THIS TRACK"),
                QueueEnd::RepeatOne => format!("REPEAT ONE{SEP}THIS TRACK REPEATS"),
                QueueEnd::Loops => format!("END OF QUEUE{SEP}REPEATS THIS TRACK"),
                QueueEnd::Reshuffles => format!("END OF QUEUE{SEP}RESHUFFLES AND STARTS OVER"),
            };
            note_row(f, 0, &text);
            return;
        }

        let tags = self.settings_mgr.settings.metadata_view;
        let index_cols = digits as u16 + if screen.tier >= WidthTier::Medium { 4 } else { 3 };
        let show_artist = tags && screen.tier >= WidthTier::Compact && summary.any_artist;
        let format_cols = if full && summary.format_cols > 0 {
            summary.format_cols + 4
        } else {
            0
        };
        let duration_cols = summary.duration_cols;
        // Title and artist share what is left about 3:2, as in the queue panel.
        let free = usize::from(list.width).saturating_sub(usize::from(index_cols) + format_cols + duration_cols);
        let title_cols = ((free * 3 + 2) / 5) as u16;

        let rows = usize::from(list.height);
        let mut dy = 0;
        for idx in summary.upcoming {
            // Loop mode runs past the end of the queue to its first track.
            if idx == 0 && dy < rows {
                note_row(f, dy, &format!("END OF QUEUE{SEP}LOOPS TO {:0digits$}", 1));
                dy += 1;
            }
            if dy >= rows {
                return;
            }
            let track = &tracks[idx];
            let title = if tags {
                kit::upper(track.display_title())
            } else {
                file_label(track)
            };
            let mut cols = vec![Col::fixed(format!("{:0digits$}", idx + 1), index_cols, theme::dim())];
            if show_artist {
                cols.push(Col::fixed(title, title_cols, theme::ink()));
                cols.push(Col::flex(kit::upper(&track.artist), theme::mid()));
            } else {
                cols.push(Col::flex(title, theme::ink()));
            }
            if format_cols > 0 {
                cols.push(Col::fixed(format_badge(track), format_cols as u16, theme::dim()));
            }
            if duration_cols > 0 {
                let length = if track.duration_seconds > 0.0 {
                    kit::duration(track.duration_seconds)
                } else {
                    String::new()
                };
                cols.push(Col::fixed(
                    right_aligned(&length, duration_cols),
                    duration_cols as u16,
                    theme::mid(),
                ));
            }
            let row = Rect::new(list.x, list.y + dy as u16, list.width, 1);
            kit::list_row(f, row, &cols, RowMark::Plain);
            dy += 1;
        }
        // Every track is listed: what happens after the last one, when that isn't a loop.
        let after = match end {
            QueueEnd::Stops => Some(format!("END OF QUEUE{SEP}PLAYBACK STOPS")),
            QueueEnd::Reshuffles => Some(format!("END OF QUEUE{SEP}RESHUFFLES AND STARTS OVER")),
            QueueEnd::RepeatOne | QueueEnd::Loops => None,
        };
        if let Some(text) = after.filter(|_| dy < rows) {
            note_row(f, dy, &text);
        }
    }
}
